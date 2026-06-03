//! Context compaction — 易腐 tool 结果清理 / 上下文裁剪。
//!
//! Spec: docs/design/agent-infra-module.md §4 上下文管理 + 压缩策略
//!
//! 压缩顺序（spec §4 丢弃 / 压缩顺序）：
//!   1. MicroClear 易腐 tool 结果（替换为 `<tool_result_stub />`）
//!   2. Summarize 尾窗外历史对话
//!   3. Drop 最旧 API round
//!   4. Reactive retry（一次压缩 + 一次重发；retry 控制在 loop_executor）
//!   5. HardLimit fail closed
//!
//! 本文件提供：
//! - `micro_clear`：替换易腐 part 为 stub（作用于 systemParts）
//! - `drop_oldest_droppable_part`：按 soft limit 丢弃最旧 droppable systemPart
//! - `compact_context(bundle, policy)`：spec §4 描述的纯计算 API（作用于 ContextBundle.systemParts）
//! - `compact_messages(messages, policy, durable_ids, ...)`：spec §4/§5 — 作用于会话 messages 的纯计算
//! - `decide_tier`：纯判定函数
//! - `estimate_context_tokens(messages, context, channel)`：spec §5 描述的纯计算 token 估算
//!
//! **边界（spec §4）**：Infra 只提供压缩「机制」，不内置业务保留策略。压缩只认两个通用信号：
//! - `ContextPart.droppable`（Runtime 注入时自己标）
//! - 消息 durable 标记（loop 从 dispatched tool 的 `ToolSpec.sideEffect == trading_write` 派生，
//!   或 `kind=summary` 检查点）—— 以 `durable_message_ids: &HashSet<String>` 传入，保持通用。

use crate::domain::agent::{
    AgentMessage, AgentMessageBlock, CompactTier, ContextBundle, ContextWindowLimits,
    ContextContent, ContextPart, ContextPartKind, MessageKind, ProviderChannel, TokenEstimate,
};
use std::collections::HashSet;

/// 压缩策略（spec §4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactPolicy {
    /// MicroClear：把易腐 realtime parts 替换为 stub。
    MicroClear,
    /// Summarize：把尾窗外 chat 摘要化。本文件实现为"drop oldest"占位（实际 summarize 模型调用归 loop executor）。
    Summarize,
    /// Drop：丢弃最旧 chat part。
    Drop,
    /// ReactiveRetry：spec §4 — 比 Summarize 更激进，直接 Drop 最老一轮 API round（包括其 chat history + tool_results）。
    ReactiveRetry,
}

/// MicroClear 结果：被替换为 stub 的易腐 part 数量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicroClearReport {
    pub stubbed_parts: u32,
    pub estimated_tokens_saved: u32,
}

/// 给易腐 tool 结果替换 stub。
///
/// Spec §2 line 261：stub 文本格式必须为 `<tool_result_stub name="..." call_id="..." ref="..." />`，
/// **必须**含 attrs，让 replay 能通过 PayloadStore 拉回。因此只对内容形如
/// `<tool_result name="X" call_id="Y" ref="Z">...</tool_result>` 的 wrapper 做 stub；
/// 非 wrapper 的 droppable parts 无 attrs 可填，不该走 stub 路径。
///
/// **作用面：`systemParts`**（单 lane 设计，spec §2 ContextBundle）。systemParts 内含身份 / 工具清单
/// （`kind=system`，`droppable=false`，永不动）+ Runtime 注入的 realtime packet（`kind=realtime`，由
/// Runtime 标 `droppable`）/ memory。处理策略：
///   - droppable + `<tool_result>` wrapper → stub 化（保留 name + call_id + ref）
///   - droppable + 非 wrapper → 直接移除（无 attrs 可填，不能走 stub）。realtime packet 是 fresh data，过期就该 drop。
///   - `droppable = false` 的 part 保留不动（spec §2 invariant，工具清单 / 身份在此）。
pub fn micro_clear(bundle: &mut ContextBundle) -> MicroClearReport {
    let mut report = MicroClearReport {
        stubbed_parts: 0,
        estimated_tokens_saved: 0,
    };
    bundle.system_parts.retain_mut(|p| {
        if !p.droppable || is_stub(p) {
            return true;
        }
        if content_is_tool_result(&p.content) {
            report.estimated_tokens_saved += p.estimated_tokens();
            stub_in_place(p);
            report.stubbed_parts += 1;
            true
        } else {
            // Non-wrapper droppable part (e.g. realtime packet): drop entirely.
            report.estimated_tokens_saved += p.estimated_tokens();
            report.stubbed_parts += 1;
            false
        }
    });
    report
}

/// True iff the part's content body contains a `<tool_result ...>` opening tag with
/// at least one of the canonical attributes (name / call_id / ref).
fn content_is_tool_result(content: &ContextContent) -> bool {
    let body = match content {
        ContextContent::Text(s) => s.clone(),
        ContextContent::Json(v) => v.to_string(),
    };
    parse_tool_result_attrs(&body).is_some()
}

fn stub_in_place(p: &mut ContextPart) {
    let text = render_stub(&p.content);
    p.token_estimate = Some(text.chars().count().div_ceil(4) as u32);
    p.content = ContextContent::Text(text);
    p.kind = ContextPartKind::ToolResultStub;
}

/// 把一段 part 内容（必须含 `<tool_result name=".." call_id=".." ref="..">...</tool_result>` wrapper）
/// 渲染成对应的 `<tool_result_stub name=".." call_id=".." ref=".." />`。
///
/// Spec §2 line 261: stub 文本格式必须为 `<tool_result_stub name="..." call_id="..." ref="..." />`，
/// **必须**含 attrs。caller 必须先用 `content_is_tool_result` 校验 wrapper 存在；非 wrapper part
/// 应在 caller 层被 drop 而不是 stub。
///
/// Spec invariant: stub 只对 tool_result wrapper 调用 —— 否则 panic。
fn render_stub(content: &ContextContent) -> String {
    let body = match content {
        ContextContent::Text(s) => s.clone(),
        ContextContent::Json(v) => v.to_string(),
    };
    let attrs = parse_tool_result_attrs(&body).expect(
        "render_stub spec invariant violated: caller must guarantee tool_result wrapper present \
         (spec §2 line 261 — stub must have name/call_id/ref attrs)",
    );
    let mut s = String::from("<tool_result_stub");
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
struct ToolResultAttrs {
    name: Option<String>,
    call_id: Option<String>,
    payload_ref: Option<String>,
}

/// 从 `<tool_result ...>...</tool_result>` 或 `<tool_result ... />` 头部提取
/// `name` / `call_id` / `ref` 属性。任何属性缺失返回 None；标签不存在也返回 None。
fn parse_tool_result_attrs(s: &str) -> Option<ToolResultAttrs> {
    let open_pos = s.find("<tool_result")?;
    let rest = &s[open_pos + "<tool_result".len()..];
    let close_gt = rest.find('>')?;
    let attrs_str = &rest[..close_gt];
    let attrs = ToolResultAttrs {
        name: read_attr(attrs_str, "name"),
        call_id: read_attr(attrs_str, "call_id"),
        payload_ref: read_attr(attrs_str, "ref"),
    };
    // Heuristic: must look like a real tool_result tag (i.e. have at least name).
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
    matches!(p.kind, ContextPartKind::ToolResultStub)
}

/// Drop 最旧的 droppable systemParts，直到落到 soft limit 以下。
///
/// Spec §4 顺序步骤 3。返回被丢弃的 part 数。`droppable=false`（工具清单 / 身份）永不丢。
pub fn drop_oldest_droppable_part(
    bundle: &mut ContextBundle,
    limits: ContextWindowLimits,
) -> u32 {
    let mut dropped = 0u32;
    while bundle.estimated_tokens() > limits.soft_limit_tokens
        && !bundle.system_parts.is_empty()
    {
        let pos = bundle.system_parts.iter().position(|p| p.droppable);
        match pos {
            Some(i) => {
                bundle.system_parts.remove(i);
                dropped += 1;
            }
            None => break,
        }
    }
    dropped
}

/// 决策下一步该走的压缩 tier。
///
/// Spec §4 触发条件表 — 按 token 估算决策（time-based MicroClear 已从 spec 删除）。
pub fn decide_tier(
    bundle: &ContextBundle,
    limits: ContextWindowLimits,
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
    None
}

/// 启发式估算一条消息的 token 数（4 字符 / token，跨所有 text / thinking block）。
pub fn estimate_message_tokens(msg: &AgentMessage) -> u32 {
    let chars: usize = msg
        .blocks
        .iter()
        .map(|b| match b {
            AgentMessageBlock::Text { text } => text.chars().count(),
            AgentMessageBlock::Thinking { text, .. } => text.chars().count(),
            // image dataRef 只是 URI 引用；wire 时才 base64，估算时按引用长度近似。
            AgentMessageBlock::Image { data_ref, .. } => data_ref.chars().count(),
        })
        .sum();
    chars.div_ceil(4) as u32
}

/// 启发式估算一批会话消息的 token 总和。
pub fn estimate_messages_tokens(messages: &[AgentMessage]) -> u32 {
    let sum: u64 = messages.iter().map(|m| u64::from(estimate_message_tokens(m))).sum();
    sum.min(u32::MAX as u64) as u32
}

/// `estimate_context_tokens(messages, context, channel)` — spec §5 描述的纯计算 token 估算。
///
/// 行为：会话 messages 估算（`estimate_messages_tokens`）+ `ContextBundle::estimated_tokens()`
/// 启发式求和（4 字符 / token），与 `channel.context_window_tokens` 对照判断是否超过 soft limit。
/// 当 channel 没有声明 `context_window_tokens` 时,使用 `ContextWindowLimits::default()` 的
/// `soft_limit_tokens` 作为软限制。
pub fn estimate_context_tokens(
    messages: &[AgentMessage],
    context: &ContextBundle,
    channel: &ProviderChannel,
) -> TokenEstimate {
    let total_tokens = context
        .estimated_tokens()
        .saturating_add(estimate_messages_tokens(messages));
    // Spec §4: soft limit drives MicroClear。channel 没声明窗口大小时退化到默认软限制。
    let soft_limit = channel
        .context_window_tokens
        .map(|w| {
            // 默认软限制按窗口的 ~33% 估算(与 ContextWindowLimits::default 60k / 180k 比例一致)。
            (w as u64).max(1) / 3
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
            let n = drop_oldest_droppable_part(&mut bundle, limits);
            (bundle, n)
        }
        CompactPolicy::ReactiveRetry => {
            // 更激进：MicroClear + 丢弃最老若干 droppable systemParts。
            // 跳过 stub —— stub 体积已极小且携带 replay ref（name/call_id/ref），不该被二次丢弃。
            let rep = micro_clear(&mut bundle);
            let mut extra = 0u32;
            for _ in 0..2 {
                if let Some(i) = bundle
                    .system_parts
                    .iter()
                    .position(|p| p.droppable && !is_stub(p))
                {
                    bundle.system_parts.remove(i);
                    extra += 1;
                } else {
                    break;
                }
            }
            (bundle, rep.stubbed_parts + extra)
        }
    }
}

// ---------------------------------------------------------------------------
// Message-lane compaction (spec §4 多轮会话持久化与续接 + §5 compact_context 作用于 messages)
// ---------------------------------------------------------------------------

/// 一条消息是否 durable（永不压缩 / 替 stub）。通用信号，不感知业务：
/// - `kind == Summary`（§4 压缩检查点）；
/// - `message_id ∈ durable_message_ids`（loop 从 dispatched tool 的 `sideEffect == trading_write`
///   派生；Infra 不知道"交易"含义，只消费 id 集合）。
pub fn message_is_durable(msg: &AgentMessage, durable_message_ids: &HashSet<String>) -> bool {
    msg.kind == Some(MessageKind::Summary) || durable_message_ids.contains(&msg.message_id)
}

/// 一条消息体是否含 `<tool_result ...>` wrapper（可被 MicroClear 替 stub）。
fn message_is_tool_result(msg: &AgentMessage) -> bool {
    msg.blocks.iter().any(|b| match b {
        AgentMessageBlock::Text { text } => parse_tool_result_attrs(text).is_some(),
        _ => false,
    })
}

/// 把一条 tool_result 消息的 text block 替换为 `<tool_result_stub .. />`（保留 name/call_id/ref）。
fn stub_message_in_place(msg: &mut AgentMessage) {
    for b in msg.blocks.iter_mut() {
        if let AgentMessageBlock::Text { text } = b {
            if parse_tool_result_attrs(text).is_some() {
                *text = render_stub(&ContextContent::Text(text.clone()));
            }
        }
    }
}

/// MicroClear（messages lane）：把尾窗（最近 `keep_recent` 条）**之外**的非 durable
/// tool_result 消息替换为 stub（保留 name/call_id/ref）。durable / summary / 非 tool_result
/// 消息原样保留。返回被 stub 化的消息数。
///
/// Spec §4：可清理内容替 stub，LLM 仍可通过 `<use_tool>` 重新拉取；不可清理项（trading_write /
/// summary）永远 inline 保留。
pub fn micro_clear_messages(
    messages: &mut [AgentMessage],
    durable_message_ids: &HashSet<String>,
    keep_recent: usize,
) -> u32 {
    let n = messages.len();
    let cutoff = n.saturating_sub(keep_recent);
    let mut stubbed = 0u32;
    for (i, msg) in messages.iter_mut().enumerate() {
        if i >= cutoff {
            break;
        }
        if message_is_durable(msg, durable_message_ids) {
            continue;
        }
        if message_is_tool_result(msg) {
            // already a stub? parse_tool_result_attrs matches stub too (it has name/call_id/ref),
            // but stub tag is `<tool_result_stub`. Guard: skip if already stubbed.
            let already_stub = msg.blocks.iter().any(|b| {
                matches!(b, AgentMessageBlock::Text { text } if text.contains("<tool_result_stub"))
            });
            if !already_stub {
                stub_message_in_place(msg);
                stubbed += 1;
            }
        }
    }
    stubbed
}

/// Drop 最旧一轮 API round（messages lane）：移除尾窗外、最旧的非 durable 消息。
/// 只清非 durable（droppable）部分；durable（trading_write / summary）保留 inline（spec §4）。
/// 返回被移除的消息数。
pub fn drop_oldest_round_messages(
    messages: &mut Vec<AgentMessage>,
    durable_message_ids: &HashSet<String>,
    keep_recent: usize,
) -> u32 {
    let cutoff = messages.len().saturating_sub(keep_recent);
    // 找尾窗外第一条非 durable 消息移除（一轮的近似：最旧 assistant + 紧随 tool_result user）。
    let mut removed = 0u32;
    let mut i = 0usize;
    while i < messages.len() && i < cutoff {
        if !message_is_durable(&messages[i], durable_message_ids) {
            messages.remove(i);
            removed += 1;
            // remove at most one "round" worth: the assistant turn + its following tool_result.
            // After removing one assistant message, also remove a following non-durable
            // tool_result user message if present (same logical round).
            if i < messages.len()
                && i < messages.len().saturating_sub(keep_recent.saturating_sub(removed as usize))
                && !message_is_durable(&messages[i], durable_message_ids)
                && message_is_tool_result(&messages[i])
            {
                messages.remove(i);
                removed += 1;
            }
            break;
        }
        i += 1;
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试辅助：构造一个 systemParts 内的 part（kind=Realtime，由 caller 标 droppable）。
    fn sys(text: &str, droppable: bool) -> ContextPart {
        ContextPart {
            kind: ContextPartKind::Realtime,
            content: ContextContent::Text(text.into()),
            freshness: None,
            token_estimate: None,
            droppable,
        }
    }

    #[test]
    fn micro_clear_replaces_tool_result_wrappers_with_stubs() {
        // Spec §2: droppable tool_result wrappers in systemParts → stub with attrs.
        let mut b = ContextBundle::new("r1");
        b.system_parts.push(sys(
            &format!(
                r#"<tool_result name="q1" call_id="tc_a" ref="pl_a">{}</tool_result>"#,
                "q".repeat(400)
            ),
            true,
        ));
        b.system_parts.push(sys(
            &format!(
                r#"<tool_result name="q2" call_id="tc_b" ref="pl_b">{}</tool_result>"#,
                "r".repeat(400)
            ),
            true,
        ));
        let before = b.estimated_tokens();
        let rep = micro_clear(&mut b);
        let after = b.estimated_tokens();
        assert_eq!(rep.stubbed_parts, 2);
        assert!(after < before);
        for p in &b.system_parts {
            assert!(is_stub(p));
        }
    }

    #[test]
    fn drop_oldest_keeps_non_droppable() {
        let mut b = ContextBundle::new("r1");
        b.system_parts.push(sys(&"a".repeat(1000), false));
        b.system_parts.push(sys(&"b".repeat(1000), true));
        b.system_parts.push(sys(&"c".repeat(1000), true));
        let limits = ContextWindowLimits {
            soft_limit_tokens: 300,
            summarize_threshold_tokens: 500,
            hard_limit_tokens: 1000,
        };
        let n = drop_oldest_droppable_part(&mut b, limits);
        assert_eq!(n, 2);
        assert_eq!(b.system_parts.len(), 1);
        assert!(!b.system_parts[0].droppable);
    }

    #[test]
    fn decide_tier_picks_summarize_above_threshold() {
        let mut b = ContextBundle::new("r1");
        b.system_parts.push(sys(&"x".repeat(4_000), true)); // ~1000 tokens
        let limits = ContextWindowLimits {
            soft_limit_tokens: 200,
            summarize_threshold_tokens: 500,
            hard_limit_tokens: 5000,
        };
        assert_eq!(decide_tier(&b, limits), Some(CompactTier::Summarize));
    }

    #[test]
    fn decide_tier_picks_micro_clear_above_soft_limit() {
        let mut b = ContextBundle::new("r1");
        b.system_parts.push(sys(&"x".repeat(1_200), true)); // ~300 tokens
        let limits = ContextWindowLimits {
            soft_limit_tokens: 200,
            summarize_threshold_tokens: 5000,
            hard_limit_tokens: 9000,
        };
        assert_eq!(decide_tier(&b, limits), Some(CompactTier::MicroClear));
        // Empty bundle under soft limit → no compaction.
        assert_eq!(decide_tier(&ContextBundle::new("r2"), limits), None);
    }

    #[test]
    fn compact_context_reactive_retry_drops_parts_and_stubs_wrappers() {
        let mut b = ContextBundle::new("r1");
        b.system_parts.push(sys(
            &format!(
                r#"<tool_result name="q" call_id="tc_q" ref="pl_q">{}</tool_result>"#,
                "q".repeat(400)
            ),
            true,
        ));
        b.system_parts.push(sys(&"a".repeat(1000), true));
        b.system_parts.push(sys(&"b".repeat(1000), true));
        let (out, n) = compact_context(b, CompactPolicy::ReactiveRetry, ContextWindowLimits::default());
        assert!(n >= 2);
        // The wrapper survives as a stub; the plain droppable parts are removed.
        assert!(out
            .system_parts
            .iter()
            .any(|p| matches!(p.kind, ContextPartKind::ToolResultStub)));
        assert!(out.system_parts.len() <= 1);
    }

    #[test]
    fn micro_clear_preserves_name_call_id_ref_in_stub() {
        // Spec §4: 易腐 tool 结果替换 stub 时，必须保留 name + call_id + ref。
        let mut b = ContextBundle::new("r1");
        b.system_parts.push(sys(
            r#"<tool_result name="fetch_quote" call_id="tc_abc" ref="pl_xyz">{"price":"1.0"}</tool_result>"#,
            true,
        ));
        let rep = micro_clear(&mut b);
        assert_eq!(rep.stubbed_parts, 1);
        let rendered = match &b.system_parts[0].content {
            ContextContent::Text(s) => s.clone(),
            _ => panic!("expected text"),
        };
        assert!(rendered.contains(r#"name="fetch_quote""#), "{}", rendered);
        assert!(rendered.contains(r#"call_id="tc_abc""#), "{}", rendered);
        assert!(rendered.contains(r#"ref="pl_xyz""#), "{}", rendered);
        assert!(rendered.starts_with("<tool_result_stub"));
        assert!(rendered.ends_with("/>"));
    }

    #[test]
    fn micro_clear_drops_non_tool_result_droppable_parts() {
        // Spec §2: stub must have name/call_id/ref attrs — non-wrapper droppable parts
        // have no attrs to fill, so they're dropped entirely instead of stubbed.
        let mut b = ContextBundle::new("r1");
        b.system_parts
            .push(sys("plain realtime packet with no tool_result tag", true));
        b.system_parts.push(sys(
            r#"<tool_result name="quote" call_id="tc_q" ref="pl_q">{"px":"1"}</tool_result>"#,
            true,
        ));
        let rep = micro_clear(&mut b);
        assert_eq!(rep.stubbed_parts, 2, "both droppable parts compacted (1 dropped + 1 stubbed)");
        // Only the wrapper part survives, as a stub with attrs:
        assert_eq!(b.system_parts.len(), 1);
        let surviving = &b.system_parts[0];
        assert!(matches!(surviving.kind, ContextPartKind::ToolResultStub));
        let rendered = match &surviving.content {
            ContextContent::Text(s) => s.clone(),
            _ => panic!("expected text"),
        };
        assert!(rendered.starts_with("<tool_result_stub"), "{}", rendered);
        assert!(rendered.contains(r#"name="quote""#), "{}", rendered);
        assert!(rendered.contains(r#"call_id="tc_q""#), "{}", rendered);
        assert!(rendered.contains(r#"ref="pl_q""#), "{}", rendered);
        assert!(rendered.ends_with("/>"), "{}", rendered);
    }

    #[test]
    fn micro_clear_keeps_droppable_false_parts() {
        // Spec §2 invariant: parts with droppable = false must never be removed or mutated
        // by compaction — this is where the tool list / identity live.
        let mut b = ContextBundle::new("r1");
        b.system_parts.push(sys("tool list / identity — must survive", false));
        b.system_parts.push(sys(
            r#"<tool_result name="quote" call_id="tc_q" ref="pl_q">{"px":"1"}</tool_result>"#,
            false,
        ));
        let rep = micro_clear(&mut b);
        assert_eq!(rep.stubbed_parts, 0);
        assert_eq!(b.system_parts.len(), 2);
        let head = match &b.system_parts[0].content {
            ContextContent::Text(s) => s.clone(),
            _ => panic!("text"),
        };
        assert_eq!(head, "tool list / identity — must survive");
    }

    #[test]
    fn parse_tool_result_attrs_handles_quoted_values() {
        let out =
            parse_tool_result_attrs(r#"<tool_result name="a" call_id="tc_1" ref="pl_2">x</tool_result>"#)
                .unwrap();
        assert_eq!(out.name.as_deref(), Some("a"));
        assert_eq!(out.call_id.as_deref(), Some("tc_1"));
        assert_eq!(out.payload_ref.as_deref(), Some("pl_2"));
    }

    #[test]
    fn parse_tool_result_attrs_returns_none_when_no_tag() {
        assert!(parse_tool_result_attrs("hello world").is_none());
    }

    #[test]
    fn estimate_context_tokens_returns_over_soft_limit_when_above() {
        use crate::domain::agent::{ProviderChannel, WireFormat};
        let mut b = ContextBundle::new("r1");
        b.system_parts.push(sys(&"x".repeat(40_000), true)); // ~10k tokens
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
        let est = estimate_context_tokens(&[], &b, &channel);
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
        let est = estimate_context_tokens(&[], &b, &channel);
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
        let est = estimate_context_tokens(&[], &b, &channel);
        // No window: default soft_limit_tokens is 60_000; empty bundle is 0 → under.
        assert!(!est.over_soft_limit);
    }

    #[test]
    fn compact_context_micro_clear_stubs_wrapper_and_drops_plain() {
        let mut b = ContextBundle::new("r1");
        b.system_parts.push(sys(
            &format!(
                r#"<tool_result name="q" call_id="tc_q" ref="pl_q">{}</tool_result>"#,
                "q".repeat(400)
            ),
            true,
        ));
        b.system_parts.push(sys(&"x".repeat(400), true)); // plain droppable → dropped
        let (out, n) = compact_context(b, CompactPolicy::MicroClear, ContextWindowLimits::default());
        assert_eq!(n, 2);
        // wrapper survives as stub, plain droppable part dropped.
        assert_eq!(out.system_parts.len(), 1);
        assert!(matches!(
            out.system_parts[0].kind,
            ContextPartKind::ToolResultStub
        ));
    }

    // ---- message-lane compaction ----

    fn amsg(id: &str, role: crate::domain::agent::AgentMessageRole, text: &str) -> AgentMessage {
        AgentMessage {
            message_id: id.into(),
            run_id: Some("r1".into()),
            conversation_id: Some("c".into()),
            seq: None,
            kind: None,
            role,
            blocks: vec![AgentMessageBlock::Text { text: text.into() }],
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn micro_clear_messages_stubs_non_durable_old_tool_results() {
        use crate::domain::agent::AgentMessageRole;
        let mut msgs = vec![
            amsg("a0", AgentMessageRole::Assistant, "thinking"),
            amsg(
                "u1",
                AgentMessageRole::User,
                r#"<tool_result name="fetch_quote" call_id="tc_1" ref="pl_1">{"px":"1"}</tool_result>"#,
            ),
            amsg(
                "u2",
                AgentMessageRole::User,
                r#"<tool_result name="operate_account" call_id="tc_2" ref="pl_2">{"orderId":"o1"}</tool_result>"#,
            ),
            amsg("a3", AgentMessageRole::Assistant, "recent"),
        ];
        // u2 is durable (trading_write); keep_recent=0 so all are candidates.
        let durable: HashSet<String> = ["u2".to_string()].into_iter().collect();
        let n = micro_clear_messages(&mut msgs, &durable, 0);
        assert_eq!(n, 1, "only u1 (non-durable tool_result) stubbed");
        // u1 stubbed, preserves name/call_id/ref
        let u1 = match &msgs[1].blocks[0] {
            AgentMessageBlock::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(u1.starts_with("<tool_result_stub"));
        assert!(u1.contains(r#"name="fetch_quote""#));
        assert!(u1.contains(r#"ref="pl_1""#));
        // u2 (durable trading_write) untouched
        let u2 = match &msgs[2].blocks[0] {
            AgentMessageBlock::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(u2.contains("<tool_result name="), "durable trading_write must stay inline");
        assert!(u2.contains("orderId"));
    }

    #[test]
    fn micro_clear_messages_respects_keep_recent_window() {
        use crate::domain::agent::AgentMessageRole;
        let mut msgs = vec![
            amsg(
                "u0",
                AgentMessageRole::User,
                r#"<tool_result name="q" call_id="tc_0" ref="pl_0">{}</tool_result>"#,
            ),
            amsg(
                "u1",
                AgentMessageRole::User,
                r#"<tool_result name="q" call_id="tc_1" ref="pl_1">{}</tool_result>"#,
            ),
        ];
        // keep_recent=1 → only u0 eligible.
        let n = micro_clear_messages(&mut msgs, &HashSet::new(), 1);
        assert_eq!(n, 1);
        let u1 = match &msgs[1].blocks[0] {
            AgentMessageBlock::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(u1.contains("<tool_result name="), "recent window kept inline");
    }

    #[test]
    fn micro_clear_messages_keeps_summary_durable() {
        use crate::domain::agent::AgentMessageRole;
        let mut summary = amsg(
            "s0",
            AgentMessageRole::Assistant,
            r#"<tool_result name="q" call_id="sc" ref="pl">{}</tool_result>"#,
        );
        summary.kind = Some(MessageKind::Summary);
        let mut msgs = vec![summary, amsg("a1", AgentMessageRole::Assistant, "x")];
        let n = micro_clear_messages(&mut msgs, &HashSet::new(), 0);
        assert_eq!(n, 0, "summary checkpoint is durable, never stubbed");
    }

    #[test]
    fn drop_oldest_round_messages_removes_oldest_non_durable() {
        use crate::domain::agent::AgentMessageRole;
        let mut msgs = vec![
            amsg("a0", AgentMessageRole::Assistant, "old assistant"),
            amsg(
                "u1",
                AgentMessageRole::User,
                r#"<tool_result name="q" call_id="tc_1" ref="pl_1">{}</tool_result>"#,
            ),
            amsg("a2", AgentMessageRole::Assistant, "recent"),
        ];
        let removed = drop_oldest_round_messages(&mut msgs, &HashSet::new(), 1);
        assert!(removed >= 1);
        // a0 (oldest) removed; the recent tail kept.
        assert!(msgs.iter().all(|m| m.message_id != "a0"));
        assert!(msgs.iter().any(|m| m.message_id == "a2"));
    }

    #[test]
    fn estimate_context_tokens_includes_messages() {
        use crate::domain::agent::{AgentMessageRole, ProviderChannel, WireFormat};
        let msgs = vec![amsg(
            "a0",
            AgentMessageRole::Assistant,
            &"x".repeat(40_000),
        )]; // ~10k tokens
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
            context_window_tokens: Some(9000), // soft = 3000
            thinking_budget_tokens: None,
        };
        let est = estimate_context_tokens(&msgs, &b, &channel);
        assert!(est.total_tokens >= 10_000, "messages must count: {}", est.total_tokens);
        assert!(est.over_soft_limit);
    }
}
