//! Context compaction — 易腐 skill 结果清理 / 上下文裁剪。
//!
//! Spec: docs/design/agent-infra-module.md §4 上下文管理 + 压缩策略
//!
//! 压缩顺序（spec §4 丢弃 / 压缩顺序）：
//!   1. MicroClear 易腐 skill 结果（替换为 `<skill_result_stub />`）
//!   2. Summarize 尾窗外历史对话
//!   3. Drop 最旧 API round
//!   4. Reactive retry（一次压缩 + 一次重发；retry 控制在 loop_executor）
//!   5. HardLimit fail closed
//!
//! 本文件提供：
//! - `micro_clear`：替换易腐 part 为 stub
//! - `drop_oldest_chat_until`：按 soft limit 丢弃最旧 chat part
//! - `compact_context(bundle, policy)`：spec §4 描述的纯计算 API
//! - `decide_tier`：纯判定函数

use crate::domain::agent::{
    CompactTier, ContextBundle, ContextContent, ContextPart, ContextPartKind, ContextWindowLimits,
};

/// 压缩策略（spec §4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactPolicy {
    /// MicroClear：把易腐 realtime parts 替换为 stub。
    MicroClear,
    /// Summarize：把尾窗外 chat 摘要化。本文件实现为"drop oldest"占位（实际 summarize 模型调用归 loop executor）。
    Summarize,
    /// Drop：丢弃最旧 chat part。
    Drop,
    /// ReactiveRetry：spec §4 — 比 Summarize 更激进，直接 Drop 最老一轮 API round（包括其 chat history + skill_results）。
    ReactiveRetry,
}

/// MicroClear 结果：被替换为 stub 的易腐 part 数量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicroClearReport {
    pub stubbed_parts: u32,
    pub estimated_tokens_saved: u32,
}

/// 给易腐 skill 结果替换 stub。
///
/// Spec §4：易腐 skill 结果替换 stub 时，**必须保留 `name` + `call_id` + `ref`**，让 replay
/// 能通过 PayloadStore 拉回。本函数扫描 droppable realtime parts，若内容形如
/// `<skill_result name="X" call_id="Y" ref="Z">...</skill_result>` 则提取属性渲染成
/// `<skill_result_stub name="X" call_id="Y" ref="Z" />`；无法识别时（无 skill_result 包裹的纯文本）
/// 退化为不含属性的 `<skill_result_stub />`，但不影响 audit 真源（`agent_skill_calls` + `agent_payloads`
/// 持久化不动）。
pub fn micro_clear(bundle: &mut ContextBundle) -> MicroClearReport {
    let mut report = MicroClearReport {
        stubbed_parts: 0,
        estimated_tokens_saved: 0,
    };
    for p in bundle.realtime_parts.iter_mut() {
        if p.droppable && !is_stub(p) {
            report.estimated_tokens_saved += p.estimated_tokens();
            stub_in_place(p);
            report.stubbed_parts += 1;
        }
    }
    report
}

fn stub_in_place(p: &mut ContextPart) {
    let text = render_stub(&p.content);
    p.token_estimate = Some(((text.chars().count() + 3) / 4) as u32);
    p.content = ContextContent::Text(text);
    p.kind = ContextPartKind::SkillResultStub;
}

/// 把一段 part 内容（可能含 `<skill_result name=".." call_id=".." ref="..">...</skill_result>`）
/// 渲染成对应的 `<skill_result_stub name=".." call_id=".." ref=".." />`。
///
/// Spec §4 line 511: 易腐 skill 结果替换 stub 时，必须保留 `name` + `call_id` + `ref`。
fn render_stub(content: &ContextContent) -> String {
    let body = match content {
        ContextContent::Text(s) => s.clone(),
        ContextContent::Json(v) => v.to_string(),
    };
    if let Some(attrs) = parse_skill_result_attrs(&body) {
        let mut s = String::from("<skill_result_stub");
        if let Some(name) = attrs.name {
            s.push_str(&format!(r#" name="{}""#, escape_attr(&name)));
        }
        if let Some(call_id) = attrs.call_id {
            s.push_str(&format!(r#" call_id="{}""#, escape_attr(&call_id)));
        }
        if let Some(payload_ref) = attrs.payload_ref {
            s.push_str(&format!(r#" ref="{}""#, escape_attr(&payload_ref)));
        }
        s.push_str(" />");
        s
    } else {
        "<skill_result_stub />".to_string()
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct SkillResultAttrs {
    name: Option<String>,
    call_id: Option<String>,
    payload_ref: Option<String>,
}

/// 从 `<skill_result ...>...</skill_result>` 或 `<skill_result ... />` 头部提取
/// `name` / `call_id` / `ref` 属性。任何属性缺失返回 None；标签不存在也返回 None。
fn parse_skill_result_attrs(s: &str) -> Option<SkillResultAttrs> {
    let open_pos = s.find("<skill_result")?;
    let rest = &s[open_pos + "<skill_result".len()..];
    let close_gt = rest.find('>')?;
    let attrs_str = &rest[..close_gt];
    let mut attrs = SkillResultAttrs::default();
    attrs.name = read_attr(attrs_str, "name");
    attrs.call_id = read_attr(attrs_str, "call_id");
    attrs.payload_ref = read_attr(attrs_str, "ref");
    // Heuristic: must look like a real skill_result tag (i.e. have at least name).
    if attrs.name.is_none() && attrs.call_id.is_none() && attrs.payload_ref.is_none() {
        return None;
    }
    Some(attrs)
}

fn read_attr(s: &str, key: &str) -> Option<String> {
    // search for ` <key>=` boundary to avoid matching attribute prefixes (e.g. "ref" ⊂ "reference")
    let mut idx = 0usize;
    while idx < s.len() {
        let sub = &s[idx..];
        let p = sub.find(key)?;
        let abs = idx + p;
        let before_ok = abs == 0
            || s.as_bytes()
                .get(abs - 1)
                .map(|b| b.is_ascii_whitespace())
                .unwrap_or(false);
        let after = &s[abs + key.len()..];
        let after_trim = after.trim_start();
        if before_ok && after_trim.starts_with('=') {
            let after_eq = after_trim[1..].trim_start();
            let quote = after_eq.chars().next()?;
            if quote != '"' && quote != '\'' {
                return None;
            }
            let inner = &after_eq[quote.len_utf8()..];
            let end = inner.find(quote)?;
            return Some(inner[..end].to_string());
        }
        idx = abs + key.len();
    }
    None
}

fn escape_attr(s: &str) -> String {
    s.replace('"', "&quot;")
}

fn is_stub(p: &ContextPart) -> bool {
    matches!(p.kind, ContextPartKind::SkillResultStub)
}

/// Drop 最旧 chat parts，直到落到 soft limit 以下。
///
/// Spec §4 顺序步骤 3。返回被丢弃的 part 数。
pub fn drop_oldest_chat_until(
    bundle: &mut ContextBundle,
    limits: ContextWindowLimits,
) -> u32 {
    let mut dropped = 0u32;
    while bundle.estimated_tokens() > limits.soft_limit_tokens
        && !bundle.chat_parts.is_empty()
    {
        let pos = bundle.chat_parts.iter().position(|p| p.droppable);
        match pos {
            Some(i) => {
                bundle.chat_parts.remove(i);
                dropped += 1;
            }
            None => break,
        }
    }
    dropped
}

/// 决策下一步该走的压缩 tier。
///
/// Spec §4 触发条件表 — 按 token 估算决策；time-based MicroClear 由 caller 提供
/// since_last_assistant_secs。
pub fn decide_tier(
    bundle: &ContextBundle,
    limits: ContextWindowLimits,
    since_last_assistant_secs: Option<u64>,
) -> Option<CompactTier> {
    let est = bundle.estimated_tokens();
    if est > limits.hard_limit_tokens {
        return Some(CompactTier::Drop);
    }
    if est > limits.summarize_threshold_tokens {
        return Some(CompactTier::Summarize);
    }
    if est > limits.soft_limit_tokens {
        return Some(CompactTier::MicroClear);
    }
    if let Some(s) = since_last_assistant_secs {
        if s > limits.micro_clear_after_secs {
            return Some(CompactTier::MicroClear);
        }
    }
    None
}

/// `compact_context(bundle, policy)` — spec §5 描述的纯计算 API。
///
/// 按 policy 排序 / 丢弃 / 替换 stub，返回 (new_bundle, dropped_count)。
/// **不**包含 retry 逻辑（spec §4 reactive retry：retry 由 loop_executor orchestration）。
pub fn compact_context(
    mut bundle: ContextBundle,
    policy: CompactPolicy,
    limits: ContextWindowLimits,
) -> (ContextBundle, u32) {
    match policy {
        CompactPolicy::MicroClear => {
            let rep = micro_clear(&mut bundle);
            (bundle, rep.stubbed_parts)
        }
        CompactPolicy::Summarize | CompactPolicy::Drop => {
            // 在没有 summarize 模型可用前，Summarize / Drop 都走 drop-oldest 占位。
            // loop executor 在真实场景需要时再调外部 compact provider。
            let n = drop_oldest_chat_until(&mut bundle, limits);
            (bundle, n)
        }
        CompactPolicy::ReactiveRetry => {
            // 更激进：MicroClear + 丢弃整个最老一轮 chat（droppable 的）。
            let rep = micro_clear(&mut bundle);
            // 丢弃最老一条 droppable chat（一轮的近似定义）
            let mut extra = 0u32;
            for _ in 0..2 {
                if let Some(i) = bundle.chat_parts.iter().position(|p| p.droppable) {
                    bundle.chat_parts.remove(i);
                    extra += 1;
                } else {
                    break;
                }
            }
            (bundle, rep.stubbed_parts + extra)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn realtime(text: &str) -> ContextPart {
        ContextPart {
            kind: ContextPartKind::Realtime,
            content: ContextContent::Text(text.into()),
            freshness: None,
            token_estimate: None,
            droppable: true,
        }
    }
    fn chat(text: &str, droppable: bool) -> ContextPart {
        ContextPart {
            kind: ContextPartKind::Chat,
            content: ContextContent::Text(text.into()),
            freshness: None,
            token_estimate: None,
            droppable,
        }
    }

    #[test]
    fn micro_clear_replaces_realtime_parts_with_stubs() {
        let mut b = ContextBundle::new("r1");
        b.realtime_parts.push(realtime(&"q".repeat(400)));
        b.realtime_parts.push(realtime(&"r".repeat(400)));
        let before = b.estimated_tokens();
        let rep = micro_clear(&mut b);
        let after = b.estimated_tokens();
        assert_eq!(rep.stubbed_parts, 2);
        assert!(after < before);
        for p in &b.realtime_parts {
            assert!(is_stub(p));
        }
    }

    #[test]
    fn drop_oldest_keeps_non_droppable() {
        let mut b = ContextBundle::new("r1");
        b.chat_parts.push(chat(&"a".repeat(1000), false));
        b.chat_parts.push(chat(&"b".repeat(1000), true));
        b.chat_parts.push(chat(&"c".repeat(1000), true));
        let limits = ContextWindowLimits {
            soft_limit_tokens: 300,
            summarize_threshold_tokens: 500,
            hard_limit_tokens: 1000,
            micro_clear_after_secs: 60,
        };
        let n = drop_oldest_chat_until(&mut b, limits);
        assert_eq!(n, 2);
        assert_eq!(b.chat_parts.len(), 1);
        assert!(!b.chat_parts[0].droppable);
    }

    #[test]
    fn decide_tier_picks_summarize_above_threshold() {
        let mut b = ContextBundle::new("r1");
        b.chat_parts.push(chat(&"x".repeat(4_000), true)); // ~1000 tokens
        let limits = ContextWindowLimits {
            soft_limit_tokens: 200,
            summarize_threshold_tokens: 500,
            hard_limit_tokens: 5000,
            micro_clear_after_secs: 60,
        };
        assert_eq!(decide_tier(&b, limits, None), Some(CompactTier::Summarize));
    }

    #[test]
    fn decide_tier_picks_micro_clear_when_idle() {
        let b = ContextBundle::new("r1");
        let limits = ContextWindowLimits::default();
        assert_eq!(
            decide_tier(&b, limits, Some(60 * 60 * 2)),
            Some(CompactTier::MicroClear)
        );
        assert_eq!(decide_tier(&b, limits, Some(10)), None);
    }

    #[test]
    fn compact_context_reactive_retry_drops_chats_and_stubs_realtime() {
        let mut b = ContextBundle::new("r1");
        b.realtime_parts.push(realtime(&"q".repeat(400)));
        b.chat_parts.push(chat(&"a".repeat(1000), true));
        b.chat_parts.push(chat(&"b".repeat(1000), true));
        let (out, n) = compact_context(b, CompactPolicy::ReactiveRetry, ContextWindowLimits::default());
        assert!(n >= 2);
        // realtime stubbed
        for p in &out.realtime_parts {
            assert!(matches!(p.kind, ContextPartKind::SkillResultStub));
        }
        // most chat parts removed
        assert!(out.chat_parts.len() <= 1);
    }

    #[test]
    fn micro_clear_preserves_name_call_id_ref_in_stub() {
        // Spec §4 line 511: 易腐 skill 结果替换 stub 时，必须保留 name + call_id + ref。
        let mut b = ContextBundle::new("r1");
        b.realtime_parts.push(ContextPart {
            kind: ContextPartKind::Realtime,
            content: ContextContent::Text(
                r#"<skill_result name="fetch_quote" call_id="sc_abc" ref="pl_xyz">{"price":"1.0"}</skill_result>"#
                    .into(),
            ),
            freshness: None,
            token_estimate: None,
            droppable: true,
        });
        let rep = micro_clear(&mut b);
        assert_eq!(rep.stubbed_parts, 1);
        let rendered = match &b.realtime_parts[0].content {
            ContextContent::Text(s) => s.clone(),
            _ => panic!("expected text"),
        };
        assert!(rendered.contains(r#"name="fetch_quote""#), "{}", rendered);
        assert!(rendered.contains(r#"call_id="sc_abc""#), "{}", rendered);
        assert!(rendered.contains(r#"ref="pl_xyz""#), "{}", rendered);
        assert!(rendered.starts_with("<skill_result_stub"));
        assert!(rendered.ends_with("/>"));
    }

    #[test]
    fn micro_clear_falls_back_to_bare_stub_when_no_attrs() {
        let mut b = ContextBundle::new("r1");
        b.realtime_parts.push(ContextPart {
            kind: ContextPartKind::Realtime,
            content: ContextContent::Text("plain realtime data with no skill_result tag".into()),
            freshness: None,
            token_estimate: None,
            droppable: true,
        });
        micro_clear(&mut b);
        let rendered = match &b.realtime_parts[0].content {
            ContextContent::Text(s) => s.clone(),
            _ => panic!("expected text"),
        };
        assert_eq!(rendered, "<skill_result_stub />");
    }

    #[test]
    fn parse_skill_result_attrs_handles_quoted_values() {
        let out =
            parse_skill_result_attrs(r#"<skill_result name="a" call_id="sc_1" ref="pl_2">x</skill_result>"#)
                .unwrap();
        assert_eq!(out.name.as_deref(), Some("a"));
        assert_eq!(out.call_id.as_deref(), Some("sc_1"));
        assert_eq!(out.payload_ref.as_deref(), Some("pl_2"));
    }

    #[test]
    fn parse_skill_result_attrs_returns_none_when_no_tag() {
        assert!(parse_skill_result_attrs("hello world").is_none());
    }

    #[test]
    fn compact_context_micro_clear_only_stubs_realtime() {
        let mut b = ContextBundle::new("r1");
        b.realtime_parts.push(realtime(&"q".repeat(400)));
        b.chat_parts.push(chat(&"x".repeat(400), true));
        let (out, n) = compact_context(b, CompactPolicy::MicroClear, ContextWindowLimits::default());
        assert_eq!(n, 1);
        assert!(matches!(
            out.realtime_parts[0].kind,
            ContextPartKind::SkillResultStub
        ));
        // chat untouched
        assert_eq!(out.chat_parts.len(), 1);
    }
}
