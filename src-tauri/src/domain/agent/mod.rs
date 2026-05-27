//! Agent Infra domain — canonical 类型。
//!
//! Spec: docs/design/agent-infra-module.md §2
//!
//! 仅放 Agent **Infra** 概念（`AgentMessage` / `ToolSpec` / `ToolCall` / `AgentEvent` /
//! `ProviderChannel` / `ContextBundle` / loop 请求 / `RunSummary`）。
//!
//! 不放 Runtime 概念（`AgentRun` / `AgentRunProfile` / `DecisionEpisode` /
//! `EvidenceRef` / `TradeIntent` / `StrategyCard` / `DecisionReview` 等）；
//! Runtime 由 Phase 3 主 agent 实现。

pub mod channel;
pub mod context;
pub mod events;
pub mod loop_request;
pub mod messages;
pub mod tools;

pub use channel::{ProviderChannel, WireFormat};
pub use context::{
    CompactTier, ContextBundle, ContextPart, ContextPartKind, ContextWindowLimits,
};
pub use events::{AgentEvent, AgentStopReason};
pub use loop_request::{AgentRunRequest, RunSummary, TokenEstimate};
pub use messages::{
    AgentMessage, AgentMessageBlock, AgentMessageRole, JsonSummary, MessageRoleBlockError,
};
pub use tools::{
    ToolCall, ToolCallId, ToolCallResult, ToolCallSource, ToolSideEffect, ToolSpec,
};
