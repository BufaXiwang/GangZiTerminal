//! Agent 子系统的核心类型契约——按职责分文件，对外仍是单 `types::*` 命名空间。
//!
//! - [`wire`]：canonical wire shape（Role / Message / Block / ToolResultContent / SystemBlock）
//! - [`request`]：pipeline 构造的 AgentRequest + 模型/思考/预算/工具定义
//! - [`event`]：agent loop 流式事件（AgentEvent / CompactTier / StopReason）
//!
//! 设计原则：
//! - 内部消息以 Anthropic content-block 形态作为 canonical（最具表达力的超集）。
//!   其他 provider 需要把自己的协议翻译到这套形态。
//! - 所有结构 serde 双向，便于直接落库 / 通过 Tauri emit 给前端。
//! - Pipeline 层只构造 [`AgentRequest`]，所有 prompt 拼装、cache 边界打点、
//!   工具列表组装都集中在 pipeline 完成。

pub mod event;
pub mod request;
pub mod wire;

pub use event::{AgentEvent, CompactTier, StopReason};
pub use request::{
    AgentOptions, AgentRequest, ContextBudget, EffortLevel, PipelineKind, ProviderKind,
    ServerSideTool, ThinkingConfig, ThinkingDisplay, ToolDef,
};
pub use wire::{Block, Message, Role, SystemBlock, ToolResultContent};
