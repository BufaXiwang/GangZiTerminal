//! Domain `agent_runtime` —— Agent Runtime canonical 类型。
//!
//! 对齐 docs/design/agent-runtime-module.md §2：
//! - `runs`：AgentRun / AgentRunProfileId / AgentRunStatus / AgentRunTrigger
//! - `decisions`：DecisionEpisode / EvidenceRef / TradeIntent / DecisionReview / StrategyCard
//! - `tools`：AgentToolName + profile → allowedTools 策略

pub mod decisions;
pub mod runs;
pub mod tools;

#[allow(unused_imports)] // 公共 contract 类型；外层 consumer 按需 import
pub use decisions::{
    DecisionEpisode, DecisionReview, EvidenceAccountSnapshot, EvidenceAccountTriggerSnapshot,
    EvidenceNewsSnapshot, EvidenceOrderSnapshot, EvidencePositionSnapshot, EvidenceQuoteSnapshot,
    EvidenceRef, EvidenceSnapshotBase, EvidenceStrategySnapshot, StrategyCard,
    ToolCallEvidenceSnapshot, TradeIntent,
};
