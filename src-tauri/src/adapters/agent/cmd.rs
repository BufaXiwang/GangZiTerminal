//! Agent Infra Tauri commands —— 提供只读 introspection 入口。
//!
//! Spec: docs/design/agent-infra-module.md §5（对外接口）
//!
//! 本 Phase 暴露的 commands：
//! - `agent_list_tools`：列出当前 ToolRegistry 注册的所有 ToolSpec（前端调试 / Runtime 自检用）。
//!
//! 注：触发 Agent run / 提交用户消息的入口由 Runtime 拥有（spec §5 末段）。
//! 本 Phase **不** 暴露 `run_agent` / `send_user_message` Tauri command —— Runtime 实现时再加。

use crate::adapters::error::CommandError;
use crate::domain::agent::ToolSpec;
use crate::infrastructure::agent::AgentInfra;
use tauri::State;

/// 列出当前 ToolRegistry 已注册的工具。
///
/// Spec: agent-infra-module.md §5 Tool Registry API（snapshot）
#[tauri::command]
#[specta::specta]
pub fn agent_list_tools(
    infra: State<'_, AgentInfra>,
) -> Result<Vec<ToolSpec>, CommandError> {
    Ok(infra.registry.snapshot_specs())
}
