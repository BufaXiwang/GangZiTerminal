//! Context compaction — 易腐工具结果清理 / 上下文裁剪。
//!
//! Spec: docs/design/agent-infra-module.md §4 上下文管理 + 压缩策略
//!
//! 压缩顺序（spec §4 丢弃 / 压缩顺序）：
//!   1. MicroClear 易腐工具结果
//!   2. Summarize 尾窗外历史对话
//!   3. Drop 最旧消息 / API round
//!   4. Reactive retry
//!   5. HardLimit fail closed
//!
//! 本文件只覆盖步骤 1（MicroClear，infra 内可独立完成）和步骤 3 / 4 的可枚举裁剪；
//! 步骤 2（Summarize 调 compact 模型）需要 provider 调用，由 loop executor 触发。

use crate::domain::agent::{
    CompactTier, ContextBundle, ContextPart, ContextPartKind, ContextWindowLimits,
};

/// MicroClear 结果：被替换为 stub 的易腐 part 数量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicroClearReport {
    pub stubbed_parts: u32,
    pub estimated_tokens_saved: u32,
}

/// 给易腐工具结果替换 stub。
///
/// Spec §4：易腐工具结果替换成 stub 时，必须保留 call id，不能破坏 provider tool_use / tool_result 配对。
/// 注：`ContextPart` 表达的是 provider request 投影，不直接持有 `tool_use_id` 字段——
/// provider adapter 在转换到 wire format 时仍以 `AgentMessage` 中的 tool_use / tool_result 配对为准。
/// 本函数只清空 `realtime`（行情 / 新闻 / 扫描）lane 中 `droppable=true` 的 part 内容。
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

/// 把 part 内容替换成一段 stub 文本（保留 kind / droppable）。
fn stub_in_place(p: &mut ContextPart) {
    p.content = crate::domain::agent::context::ContextContent::Text(
        "[compacted: perishable tool result cleared]".into(),
    );
    p.token_estimate = Some(8);
    p.kind = ContextPartKind::ToolStub;
}

fn is_stub(p: &ContextPart) -> bool {
    matches!(p.kind, ContextPartKind::ToolStub)
}

/// Drop 最旧 chat parts，直到落到 soft limit 以下。
///
/// Spec §4 顺序步骤 3：Drop 最旧消息 / API round。
/// 返回被丢弃的 part 数。
pub fn drop_oldest_chat_until(
    bundle: &mut ContextBundle,
    limits: ContextWindowLimits,
) -> u32 {
    let mut dropped = 0u32;
    while bundle.estimated_tokens() > limits.soft_limit_tokens
        && !bundle.chat_parts.is_empty()
    {
        // 跳过 droppable=false 的 part；这些必须保留。
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
/// Spec §4 触发条件表 — 单纯按 token 估算决策；time-based MicroClear 由 caller 提供 since_last_assistant_secs。
pub fn decide_tier(
    bundle: &ContextBundle,
    limits: ContextWindowLimits,
    since_last_assistant_secs: Option<u64>,
) -> Option<CompactTier> {
    let est = bundle.estimated_tokens();
    if est > limits.hard_limit_tokens {
        // 仍超过 hard limit — caller 必须 fail closed。
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::context::ContextContent;

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
        // both parts now ToolStub
        for p in &b.realtime_parts {
            assert!(is_stub(p));
        }
    }

    #[test]
    fn drop_oldest_keeps_non_droppable() {
        let mut b = ContextBundle::new("r1");
        // 1000 chars chat parts -> ~250 token each
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
        // the only one left must be the non-droppable
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
        // Idle 2h, while micro_clear_after_secs default = 1h
        assert_eq!(
            decide_tier(&b, limits, Some(60 * 60 * 2)),
            Some(CompactTier::MicroClear)
        );
        assert_eq!(decide_tier(&b, limits, Some(10)), None);
    }
}
