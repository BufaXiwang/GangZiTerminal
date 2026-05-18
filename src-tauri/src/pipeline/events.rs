//! Pipeline → 前端事件名 + 状态广播 helper。
//!
//! 事件名集中在这里是为了让前端的 `useAppEvents` hook 能对照——改名时一次性同步。

use serde_json::json;
use tauri::{AppHandle, Emitter};

/// Agent loop 状态广播（loading / running / done）。前端在状态条显示当前阶段。
pub const EVENT_AGENT_STATUS: &str = "agent-status";

/// 行情拉取状态——只在"有问题"时 emit（成功不打扰）。
pub const EVENT_QUOTES_FETCH_STATUS: &str = "quotes-fetch-status";

pub fn emit_status(app: &AppHandle, phase: &str, message: &str) {
    let _ = app.emit(
        EVENT_AGENT_STATUS,
        json!({ "phase": phase, "message": message }),
    );
}
