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
//! - `estimate_context_tokens(context, channel)`：spec §5 描述的纯计算 token 估算

use crate::domain::agent::{
    CompactTier, ContextBundle, ContextWindowLimits, ContextContent, ContextPart, ContextPartKind,
    ProviderChannel, TokenEstimate,
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
/// Spec §2 line 261：stub 文本格式必须为 `<skill_result_stub name="..." call_id="..." ref="..." />`，
/// **必须**含 attrs，让 replay 能通过 PayloadStore 拉回。因此只对内容形如
/// `<skill_result name="X" call_id="Y" ref="Z">...</skill_result>` 的 wrapper 做 stub；
/// 非 wrapper 的 droppable parts 无 attrs 可填，不该走 stub 路径。
///
/// **Realtime lane**（spec §4 line 322）：trigger / 账户 / 行情 / 新闻 / 策略——这些不一定是
/// skill_result wrapper。处理策略：
///   - 是 `<skill_result>` wrapper → stub 化（保留 name + call_id + ref）
///   - 不是 wrapper 但 droppable → 直接移除（drop，不 stub）。realtime lane 是 fresh data，过期就该 drop。
///   - `droppable = false` 的 part 保留不动（spec §2 line 332 invariant）。
///
/// **Chat lane**（spec §4 line 442）：历史 skill_result。只对 wrapper 做 stub，纯用户 / 助理对话留给
/// Drop / Summarize 处理。
pub fn micro_clear(bundle: &mut ContextBundle) -> MicroClearReport {
    let mut report = MicroClearReport {
        stubbed_parts: 0,
        estimated_tokens_saved: 0,
    };
    // Realtime parts:
    //   - droppable + skill_result wrapper → stub 化（attrs 完整）
    //   - droppable + 非 wrapper → 直接移除（无 attrs 可填，不能走 stub）
    //   - 非 droppable → 保留
    bundle.realtime_parts.retain_mut(|p| {
        if !p.droppable || is_stub(p) {
            return true;
        }
        if content_is_skill_result(&p.content) {
            report.estimated_tokens_saved += p.estimated_tokens();
            stub_in_place(p);
            report.stubbed_parts += 1;
            true
        } else {
            // Non-wrapper realtime part: drop entirely (counted in stubbed_parts as "compacted").
            report.estimated_tokens_saved += p.estimated_tokens();
            report.stubbed_parts += 1;
            false
        }
    });
    // Chat parts: only stub-replace droppable parts whose content is itself a
    // <skill_result ...> wrapper (historical skill_results migrated into chat history).
    // Pure user / assistant chat is left untouched — Drop / Summarize handle that lane.
    for p in bundle.chat_parts.iter_mut() {
        if p.droppable && !is_stub(p) && content_is_skill_result(&p.content) {
            report.estimated_tokens_saved += p.estimated_tokens();
            stub_in_place(p);
            report.stubbed_parts += 1;
        }
    }
    report
}

/// True iff the part's content body contains a `<skill_result ...>` opening tag with
/// at least one of the canonical attributes (name / call_id / ref).
fn content_is_skill_result(content: &ContextContent) -> bool {
    let body = match content {
        ContextContent::Text(s) => s.clone(),
        ContextContent::Json(v) => v.to_string(),
    };
    parse_skill_result_attrs(&body).is_some()
}

fn stub_in_place(p: &mut ContextPart) {
    let text = render_stub(&p.content);
    p.token_estimate = Some(((text.chars().count() + 3) / 4) as u32);
    p.content = ContextContent::Text(text);
    p.kind = ContextPartKind::SkillResultStub;
}

/// 把一段 part 内容（必须含 `<skill_result name=".." call_id=".." ref="..">...</skill_result>` wrapper）
/// 渲染成对应的 `<skill_result_stub name=".." call_id=".." ref=".." />`。
///
/// Spec §2 line 261: stub 文本格式必须为 `<skill_result_stub name="..." call_id="..." ref="..." />`，
/// **必须**含 attrs。caller 必须先用 `content_is_skill_result` 校验 wrapper 存在；非 wrapper part
/// 应在 caller 层被 drop 而不是 stub。
///
/// Spec invariant: stub 只对 skill_result wrapper 调用 —— 否则 panic。
fn render_stub(content: &ContextContent) -> String {
    let body = match content {
        ContextContent::Text(s) => s.clone(),
        ContextContent::Json(v) => v.to_string(),
    };
    let attrs = parse_skill_result_attrs(&body).expect(
        "render_stub spec invariant violated: caller must guarantee skill_result wrapper present \
         (spec §2 line 261 — stub must have name/call_id/ref attrs)",
    );
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

/// `estimate_context_tokens(context, channel)` — spec §5 描述的纯计算 token 估算。
///
/// 行为：按 `ContextBundle::estimated_tokens()` 启发式求和（4 字符 / token），
/// 与 `channel.context_window_tokens` 对照判断是否超过 soft limit。
/// 当 channel 没有声明 `context_window_tokens` 时,使用 `ContextWindowLimits::default()` 的
/// `soft_limit_tokens` 作为软限制。
pub fn estimate_context_tokens(
    context: &ContextBundle,
    channel: &ProviderChannel,
) -> TokenEstimate {
    let total_tokens = context.estimated_tokens();
    // Spec §4: soft limit drives MicroClear。channel 没声明窗口大小时退化到默认软限制。
    let soft_limit = channel
        .context_window_tokens
        .map(|w| {
            // 默认软限制按窗口的 ~33% 估算(与 ContextWindowLimits::default 60k / 180k 比例一致)。
            (w as u64 * 1).max(1) / 3
        })
        .map(|v| v.min(u32::MAX as u64) as u32)
        .unwrap_or(ContextWindowLimits::default().soft_limit_tokens);
    TokenEstimate {
        total_tokens,
        over_soft_limit: total_tokens > soft_limit,
    }
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
    fn micro_clear_replaces_realtime_skill_result_wrappers_with_stubs() {
        // Spec §2 line 261: realtime droppable skill_result wrappers → stub with attrs.
        let mut b = ContextBundle::new("r1");
        b.realtime_parts.push(ContextPart {
            kind: ContextPartKind::Realtime,
            content: ContextContent::Text(format!(
                r#"<skill_result name="q1" call_id="sc_a" ref="pl_a">{}</skill_result>"#,
                "q".repeat(400)
            )),
            freshness: None,
            token_estimate: None,
            droppable: true,
        });
        b.realtime_parts.push(ContextPart {
            kind: ContextPartKind::Realtime,
            content: ContextContent::Text(format!(
                r#"<skill_result name="q2" call_id="sc_b" ref="pl_b">{}</skill_result>"#,
                "r".repeat(400)
            )),
            freshness: None,
            token_estimate: None,
            droppable: true,
        });
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
        b.realtime_parts.push(ContextPart {
            kind: ContextPartKind::Realtime,
            content: ContextContent::Text(format!(
                r#"<skill_result name="q" call_id="sc_q" ref="pl_q">{}</skill_result>"#,
                "q".repeat(400)
            )),
            freshness: None,
            token_estimate: None,
            droppable: true,
        });
        b.chat_parts.push(chat(&"a".repeat(1000), true));
        b.chat_parts.push(chat(&"b".repeat(1000), true));
        let (out, n) = compact_context(b, CompactPolicy::ReactiveRetry, ContextWindowLimits::default());
        assert!(n >= 2);
        // realtime wrapper stubbed (survives as stub)
        assert_eq!(out.realtime_parts.len(), 1);
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
    fn micro_clear_stubs_chat_skill_results_but_leaves_plain_chat_alone() {
        // Spec §4: chat history that contains historical skill_result must be stub-replaced
        // by MicroClear (preserves replay link); plain user / assistant chat is left untouched.
        let mut b = ContextBundle::new("r1");
        b.chat_parts.push(ContextPart {
            kind: ContextPartKind::Chat,
            content: ContextContent::Text(
                r#"<skill_result name="news_search" call_id="sc_news" ref="pl_news">{"items":[]}</skill_result>"#
                    .into(),
            ),
            freshness: None,
            token_estimate: None,
            droppable: true,
        });
        b.chat_parts.push(ContextPart {
            kind: ContextPartKind::Chat,
            content: ContextContent::Text("user said something".into()),
            freshness: None,
            token_estimate: None,
            droppable: true,
        });
        let rep = micro_clear(&mut b);
        assert_eq!(rep.stubbed_parts, 1);
        // First chat part stubbed:
        let head = match &b.chat_parts[0].content {
            ContextContent::Text(s) => s.clone(),
            _ => panic!("text"),
        };
        assert!(head.starts_with("<skill_result_stub"));
        assert!(head.contains(r#"name="news_search""#));
        assert!(head.contains(r#"ref="pl_news""#));
        // Second chat part left as-is:
        let tail = match &b.chat_parts[1].content {
            ContextContent::Text(s) => s.clone(),
            _ => panic!("text"),
        };
        assert_eq!(tail, "user said something");
        assert!(matches!(b.chat_parts[1].kind, ContextPartKind::Chat));
    }

    #[test]
    fn micro_clear_drops_non_skill_result_realtime_parts() {
        // Spec §2 line 261: stub must have name/call_id/ref attrs — non-wrapper realtime parts
        // have no attrs to fill, so they're dropped entirely instead of stubbed.
        // (Spec §4 line 322: realtime lane is fresh data; expired non-wrapper data should drop.)
        let mut b = ContextBundle::new("r1");
        b.realtime_parts.push(ContextPart {
            kind: ContextPartKind::Realtime,
            content: ContextContent::Text("plain realtime data with no skill_result tag".into()),
            freshness: None,
            token_estimate: None,
            droppable: true,
        });
        b.realtime_parts.push(ContextPart {
            kind: ContextPartKind::Realtime,
            content: ContextContent::Text(
                r#"<skill_result name="quote" call_id="sc_q" ref="pl_q">{"px":"1"}</skill_result>"#
                    .into(),
            ),
            freshness: None,
            token_estimate: None,
            droppable: true,
        });
        let rep = micro_clear(&mut b);
        assert_eq!(rep.stubbed_parts, 2, "both droppable parts compacted (1 dropped + 1 stubbed)");
        // Only the wrapper part survives, as a stub with attrs:
        assert_eq!(b.realtime_parts.len(), 1);
        let surviving = &b.realtime_parts[0];
        assert!(matches!(surviving.kind, ContextPartKind::SkillResultStub));
        let rendered = match &surviving.content {
            ContextContent::Text(s) => s.clone(),
            _ => panic!("expected text"),
        };
        assert!(rendered.starts_with("<skill_result_stub"), "{}", rendered);
        assert!(rendered.contains(r#"name="quote""#), "{}", rendered);
        assert!(rendered.contains(r#"call_id="sc_q""#), "{}", rendered);
        assert!(rendered.contains(r#"ref="pl_q""#), "{}", rendered);
        assert!(rendered.ends_with("/>"), "{}", rendered);
    }

    #[test]
    fn micro_clear_keeps_droppable_false_realtime_parts() {
        // Spec §2 line 332 invariant: parts with droppable = false must never be removed
        // or mutated by compaction — even by realtime-lane micro_clear.
        let mut b = ContextBundle::new("r1");
        b.realtime_parts.push(ContextPart {
            kind: ContextPartKind::Realtime,
            content: ContextContent::Text("system trigger payload — must survive".into()),
            freshness: None,
            token_estimate: None,
            droppable: false,
        });
        b.realtime_parts.push(ContextPart {
            kind: ContextPartKind::Realtime,
            content: ContextContent::Text(
                r#"<skill_result name="quote" call_id="sc_q" ref="pl_q">{"px":"1"}</skill_result>"#
                    .into(),
            ),
            freshness: None,
            token_estimate: None,
            droppable: false,
        });
        let rep = micro_clear(&mut b);
        assert_eq!(rep.stubbed_parts, 0);
        assert_eq!(b.realtime_parts.len(), 2);
        // Both untouched (not stubbed, not dropped):
        assert!(matches!(b.realtime_parts[0].kind, ContextPartKind::Realtime));
        assert!(matches!(b.realtime_parts[1].kind, ContextPartKind::Realtime));
        let head = match &b.realtime_parts[0].content {
            ContextContent::Text(s) => s.clone(),
            _ => panic!("text"),
        };
        assert_eq!(head, "system trigger payload — must survive");
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
    fn estimate_context_tokens_returns_over_soft_limit_when_above() {
        use crate::domain::agent::{ProviderChannel, WireFormat};
        let mut b = ContextBundle::new("r1");
        b.chat_parts.push(chat(&"x".repeat(40_000), true)); // ~10k tokens
        let channel = ProviderChannel {
            channel_id: "c".into(),
            provider: "p".into(),
            wire_format: WireFormat::Messages,
            base_url: None,
            api_key: String::new(),
            model: "m".into(),
            stream: true,
            enabled: true,
            supports_vision: false,
            supports_thinking: false,
            max_output_tokens: None,
            context_window_tokens: Some(9000), // soft = 9000/3 = 3000
            thinking_budget_tokens: None,
        };
        let est = estimate_context_tokens(&b, &channel);
        assert!(est.total_tokens >= 10_000);
        assert!(est.over_soft_limit);
    }

    #[test]
    fn estimate_context_tokens_under_soft_limit_when_small() {
        use crate::domain::agent::{ProviderChannel, WireFormat};
        let b = ContextBundle::new("r1");
        let channel = ProviderChannel {
            channel_id: "c".into(),
            provider: "p".into(),
            wire_format: WireFormat::Messages,
            base_url: None,
            api_key: String::new(),
            model: "m".into(),
            stream: true,
            enabled: true,
            supports_vision: false,
            supports_thinking: false,
            max_output_tokens: None,
            context_window_tokens: Some(200_000),
            thinking_budget_tokens: None,
        };
        let est = estimate_context_tokens(&b, &channel);
        assert_eq!(est.total_tokens, 0);
        assert!(!est.over_soft_limit);
    }

    #[test]
    fn estimate_context_tokens_falls_back_to_default_limits_without_window() {
        use crate::domain::agent::{ProviderChannel, WireFormat};
        let b = ContextBundle::new("r1");
        let channel = ProviderChannel {
            channel_id: "c".into(),
            provider: "p".into(),
            wire_format: WireFormat::Messages,
            base_url: None,
            api_key: String::new(),
            model: "m".into(),
            stream: true,
            enabled: true,
            supports_vision: false,
            supports_thinking: false,
            max_output_tokens: None,
            context_window_tokens: None,
            thinking_budget_tokens: None,
        };
        let est = estimate_context_tokens(&b, &channel);
        // No window: default soft_limit_tokens is 60_000; empty bundle is 0 → under.
        assert!(!est.over_soft_limit);
    }

    #[test]
    fn compact_context_micro_clear_only_stubs_realtime() {
        let mut b = ContextBundle::new("r1");
        b.realtime_parts.push(ContextPart {
            kind: ContextPartKind::Realtime,
            content: ContextContent::Text(format!(
                r#"<skill_result name="q" call_id="sc_q" ref="pl_q">{}</skill_result>"#,
                "q".repeat(400)
            )),
            freshness: None,
            token_estimate: None,
            droppable: true,
        });
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
