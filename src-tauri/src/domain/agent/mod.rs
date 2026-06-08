//! Agent Infra domain — canonical 类型。
//!
//! Spec: docs/design/agent-infra-module.md §2
//!
//! Infra 概念（`AgentMessage` / `ToolSpec` / `ToolCall` / `AgentEvent` /
//! `ProviderChannel` / `ContextBundle` / loop 请求 / `RunSummary`）见各 infra 子模块。
//!
//! Runtime 概念（`AgentRun` / `InvestmentStrategy` / `AnalysisResult` / `AgentTrade`）
//! 见 [`runtime`]（Phase 3 主 agent 实现，spec agent-runtime-module.md §3）。

pub mod channel;
pub mod context;
pub mod events;
pub mod loop_request;
pub mod messages;
pub mod runtime;
pub mod tools;

pub use channel::{ProviderChannel, WireFormat};
pub use context::{
    CompactTier, ContextBundle, ContextContent, ContextPart, ContextPartKind,
    ContextWindowLimits,
};
pub use events::{AgentEvent, AgentStopReason, CompactedTier, UsageBreakdown};
pub use loop_request::{
    AgentRunRequest, CompactionConfig, RetryConfig, RunSummary, TokenEstimate,
};
pub use messages::{
    AgentMessage, AgentMessageBlock, AgentMessageRole, JsonSummary, MessageKind,
    MessageRoleBlockError,
};
pub use runtime::{
    AccountResultRef, AgentRun, AgentRunMode, AgentRunStatus, AgentRunTrigger, AgentTrade,
    AgentTradeStatus, AnalysisResult, AnalysisResultKind, InvestmentStrategy, ReviewSuggestion,
    StrategyStatus,
};
pub use tools::{SideEffect, ToolCall, ToolCallId, ToolCallResult, ToolSpec};
