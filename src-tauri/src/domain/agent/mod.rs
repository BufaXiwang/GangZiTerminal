#![allow(dead_code, unused_imports)] // canonical 类型完整面：部分字段/variants 按 wire format 需要保留

//! Domain `agent`——Agent 决策子域。
//!
//! 当前只放 `types`（Block / Message / AgentEvent / AgentRequest 等 canonical 形态，
//! 贴合 Anthropic Messages API 的 wire shape，是 provider / tool / loop 三方共识）。
//!
//! 后续可拆出：
//! - `errors.rs`：AgentError / ProviderError / ToolError（目前散在各 file）
//! - `provider.rs`：ChatProvider trait + ProviderEvent（目前在 infrastructure）
//! - `tool_spec.rs`：Tool trait（目前在 infrastructure）
//!
//! identity.md（Agent 人设档案）目前仍由 `prompt.rs` 用 `include_str!` 直接读，
//! 保留在 src/ 顶层避免 prompt.rs 同步搬动；后续 prompt 重构时一并迁移。

pub mod memory;
pub mod types;
