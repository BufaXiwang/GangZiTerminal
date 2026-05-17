//! 应用状态相关的 Tauri IPC commands。
//!
//! - `initialize_database`：应用启动时打开 SQLite 文件 + 跑 migrate；返回 path + schema 版本
//! - `save_app_state` / `load_app_state`：前端通用 KV 写入 / 读取（用户设置等）

use crate::infrastructure::app_state::{load_app_state_value, save_app_state_value};
use crate::infrastructure::db::{
    database_path, migrate, open_database, DatabaseInfo, SCHEMA_VERSION,
};
use serde_json::Value;
use tauri::AppHandle;

#[tauri::command]
pub fn initialize_database(app: AppHandle) -> Result<DatabaseInfo, String> {
    let path = database_path(&app)?;
    let connection = open_database(&app)?;
    migrate(&connection)?;
    Ok(DatabaseInfo {
        path: path.to_string_lossy().to_string(),
        schema_version: SCHEMA_VERSION,
    })
}

#[tauri::command]
pub fn save_app_state(app: AppHandle, key: String, value: Value) -> Result<(), String> {
    save_app_state_value(&app, &key, &value)
}

#[tauri::command]
pub fn load_app_state(app: AppHandle, key: String) -> Result<Option<Value>, String> {
    load_app_state_value(&app, &key)
}
