//! Agent Infra Tauri commands —— 提供只读 introspection 入口。
//!
//! Spec: docs/design/agent-infra-module.md §5（对外接口）
//!
//! 本 Phase 暴露的 commands：
//! - `agent_list_skills`：列出当前 SkillRegistry 注册的所有 SkillSpec（前端调试 / Runtime 自检用）。
//!
//! 注：触发 Agent run / 提交用户消息的入口由 Runtime 拥有（spec §5 末段）。

use crate::adapters::error::CommandError;
use crate::domain::agent::SkillSpec;
use crate::infrastructure::agent::AgentInfra;
use tauri::State;

/// 列出当前 SkillRegistry 已注册的 skill。
///
/// Spec: agent-infra-module.md §5 Skill Registry API（snapshot）
#[tauri::command]
#[specta::specta]
pub fn agent_list_skills(
    infra: State<'_, AgentInfra>,
) -> Result<Vec<SkillSpec>, CommandError> {
    Ok(infra.registry.list_skills())
}
