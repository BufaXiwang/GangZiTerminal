#![allow(dead_code, unused_imports)] // canonical 类型完整面：部分字段/variants 按 wire format 需要保留

//! Domain `agent` —— Agent Infra 核心类型（canonical wire format 中立）。
//!
//! 对齐 docs/design/agent-infra-module.md：
//! - `types`：`AgentMessage` / `AgentMessageBlock` / `ToolSpec` / `ToolCall` /
//!   `AgentEvent` / `ProviderChannel` / `ContextBundle` 等 canonical 类型
//!   （当前用旧名 Block / Message / AgentRequest，后续可重命名）。
//!
//! Agent Runtime 业务类型（AgentRun / AgentRunProfile / DecisionEpisode /
//! EvidenceRef / TradeIntent / DecisionReview / StrategyCard）见
//! [`crate::pipeline::agent_runtime::decisions`]。
//!
//! `ChatProvider` trait 在 `infrastructure::agent::provider`，`Tool` trait 在
//! `pipeline::agent::tools`——两者都是协议适配。

pub mod types;

pub use types::{AgentMessage, MessageRole, ProviderChannel, ProviderKind, WireFormat};
// SignalKind / EventKind 等迁到 domain/shared::signal（被 Account / Quotes 复用）。
// 这里 re-export 让历史 use 路径仍可工作。
pub use crate::domain::shared::{EventKind, SignalDetection, SignalKind};
