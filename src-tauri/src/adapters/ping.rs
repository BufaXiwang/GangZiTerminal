//! 端到端联通验证命令。Phase 0 验收用。
//!
//! 不属于任何 BC——纯粹用于确认 specta bindings 生成 + Tauri invoke + 前端通信链路工作。

use crate::adapters::error::CommandError;
use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PingResult {
    pub ok: bool,
    pub message: String,
}

#[tauri::command]
#[specta::specta]
pub fn ping() -> Result<PingResult, CommandError> {
    Ok(PingResult {
        ok: true,
        message: "GangZi backend alive.".to_string(),
    })
}
