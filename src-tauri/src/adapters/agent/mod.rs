//! Agent adapters — Agent Infra Tauri command + event 命名空间。
//!
//! Spec: docs/design/agent-infra-module.md §5（对外接口 / Tool Registry API）
//!
//! 本目录暴露：
//! - `cmd::agent_list_tools`：introspection command
//! - `events::AGENT_EVENT` + `wrap_agent_event`：前端 listen 用的事件名常量
//!
//! Runtime（Phase 3）会向 `AgentInfra.registry` 注入 Quotes / News / Account facade tool，
//! 并新增 `run_agent` / `send_user_message` 等 command —— 见 agent-runtime-module.md。

pub mod cmd;
pub mod events;

pub use events::{wrap_agent_event, AGENT_EVENT};
