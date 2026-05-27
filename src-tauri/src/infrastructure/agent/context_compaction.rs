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
/// 能通过 PayloadStore 拉回。本函数把 droppable realtime parts 替换成 `<skill_result_stub />`，
/// 具体 name / call_id / ref 由 caller 注入到 part 内容（caller 知道哪些 part 是 skill_result）。
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
    // 通用 stub 占位：caller 可在替换前/后注入更精确的 name + call_id + ref。
    p.content = ContextContent::Text("<skill_result_stub />".into());
    p.token_estimate = Some(4);
    p.kind = ContextPartKind::SkillResultStub;
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
