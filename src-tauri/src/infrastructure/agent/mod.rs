//! Agent Infra infrastructure 层 —— provider channel / ToolRegistry / 持久化 / loop。
//!
//! Spec: docs/design/agent-infra-module.md §3 / §4 / §5
//!
//! 模块拆分：
//! - `migrations`         — agent_* 表 schema（messages / tool_calls / provider_channels）
//! - `messages_repo`      — `AgentMessage` / `ToolCall` 持久化
//! - `tool_registry`      — 协议级 local tool 注册 + dispatch + 超时控制
//! - `context_compaction` — 易腐工具结果清理 + 上下文裁剪
//! - `providers`          — 三类 wire format adapter（Anthropic / OpenAI Responses / Chat Completions）
//! - `loop_executor`      — canonical Agent loop

pub mod context_compaction;
pub mod loop_executor;
pub mod messages_repo;
pub mod migrations;
pub mod providers;
pub mod tool_registry;

pub use messages_repo::AgentMessagesRepo;
pub use migrations::migrations;
pub use tool_registry::{
    FnToolHandler, InputValidator, ToolHandler, ToolHandlerFuture, ToolHandlerOutput,
    ToolInvocation, ToolRegistry,
};

use std::sync::Arc;

/// Agent Infra bootstrap — 创建 Tauri State 用的 `Arc<ToolRegistry>` + repo。
///
/// Runtime 在 setup 时调用，把结果 manage 进 Tauri State；
/// 之后用 `register_tool` 注入 Quotes / News / Account facade 工具。
pub fn bootstrap(db: crate::infrastructure::db::AppDb) -> AgentInfra {
    let repo = AgentMessagesRepo::new(db);
    let registry = Arc::new(ToolRegistry::new(repo.clone()));
    AgentInfra { repo, registry }
}

/// Agent Infra 容器，由 setup 持有。
#[derive(Clone)]
pub struct AgentInfra {
    pub repo: AgentMessagesRepo,
    pub registry: Arc<ToolRegistry>,
}
