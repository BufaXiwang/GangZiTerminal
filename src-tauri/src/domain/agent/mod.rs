#![allow(dead_code, unused_imports)] // canonical 类型完整面：部分字段/variants 按 wire format 需要保留

//! Domain `agent`——Agent 决策子域。
//!
//! - `types`：Block / Message / AgentEvent / AgentRequest 等 canonical 形态，贴合
//!   Anthropic Messages API 的 wire shape，是 provider / tool / loop 三方共识
//! - `memory`：InvestorMemory（投资记忆实体 + 合并规则）
//!
//! `ChatProvider` trait 在 `infrastructure::agent::provider`，`Tool` trait 在
//! `adapters::agent_tools`——两者都是协议适配，不属于 domain。
//! identity.md（Agent 人设档案）在 `pipeline::agent`，由 `prompt.rs` `include_str!` 读入。
//!
//! 后续可拆 `errors.rs` 收拢 AgentError / ProviderError / ToolError（目前散在各 file）。

pub mod memory;
pub mod types;

pub use types::ProviderKind;
