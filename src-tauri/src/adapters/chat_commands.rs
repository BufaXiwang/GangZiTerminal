//! Chat Tauri commands.
//!
//! 这一层是 frontend IPC 的唯一入口；pipeline / repository 不直接暴露
//! `#[tauri::command]`，避免 UI 绕过 use case 边界。
//!
//! 同时是 LLM 工具的 *composition root*：每次 chat run 启动前在这里 `build_chat_registry`
//! 把所有具体 tool 实例化，再注入 pipeline。pipeline 只依赖 `Tool` 抽象，不知道有哪些
//! 具体工具——这条注入方向把"协议 ↔ 领域"的反腐译码留在 adapter 层。

use crate::adapters::agent_tools::build_chat_registry;
use crate::pipeline::chat::ChatReplyResult;
use serde_json::Value;
use std::sync::Arc;
use tauri::AppHandle;

#[tauri::command]
pub async fn send_chat_message_now(
    app: AppHandle,
    content: String,
    #[allow(non_snake_case)] images: Option<Vec<String>>,
) -> Result<ChatReplyResult, String> {
    let registry = Arc::new(build_chat_registry(&app));
    crate::pipeline::chat::send_chat_message_now(app, content, images, registry).await
}

#[tauri::command]
pub fn list_chat_messages(
    app: AppHandle,
    before: Option<String>,
    limit: Option<i64>,
) -> Result<Vec<Value>, String> {
    crate::infrastructure::agent::repository::list_chat_messages(app, before, limit)
}

#[tauri::command]
pub fn search_chat_messages(
    app: AppHandle,
    query: String,
    limit: Option<i64>,
) -> Result<Vec<Value>, String> {
    crate::infrastructure::agent::repository::search_chat_messages(app, query, limit)
}
