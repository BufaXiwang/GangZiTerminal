//! 投资记忆（InvestorMemory）的 app_state KV 读写。
//!
//! Domain 类型在 `domain::agent::memory`；这里只负责持久化适配，
//! 让 chat pipeline 和 memory tools 共用同一把 key。

use crate::domain::agent::memory::{default_investor_memory, InvestorMemory};
use tauri::AppHandle;

pub const KEY_INVESTOR_MEMORY: &str = "gangzi-terminal.investor-memory";

pub fn read_investor_memory(app: &AppHandle) -> InvestorMemory {
    match crate::infrastructure::app_state::load_app_state_value(app, KEY_INVESTOR_MEMORY) {
        Ok(Some(value)) => serde_json::from_value::<InvestorMemory>(value)
            .unwrap_or_else(|_| default_investor_memory()),
        _ => default_investor_memory(),
    }
}

pub fn save_investor_memory(app: &AppHandle, memory: &InvestorMemory) -> Result<(), String> {
    let value = serde_json::to_value(memory).map_err(|e| format!("memory 序列化失败：{e}"))?;
    crate::infrastructure::app_state::save_app_state_value(app, KEY_INVESTOR_MEMORY, &value)
}
