//! SQLite 连接 + 数据库文件路径 + DatabaseInfo DTO。

use rusqlite::Connection;
use serde::Serialize;
use std::fs;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

pub const SCHEMA_VERSION: i64 = 1;

/// SQLite 初始化的返回值——给前端 hydrate path + schema 版本号。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseInfo {
    pub path: String,
    pub schema_version: i64,
}

pub fn database_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("获取应用数据目录失败：{err}"))?;
    fs::create_dir_all(&dir).map_err(|err| format!("创建应用数据目录失败：{err}"))?;
    Ok(dir.join("gangzi-terminal.sqlite3"))
}

pub fn open_database(app: &AppHandle) -> Result<Connection, String> {
    let path = database_path(app)?;
    static LOGGED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let _ = LOGGED.get_or_init(|| {
        tracing::info!(path = %path.display(), "SQLite 数据库路径");
    });
    let connection = Connection::open(path).map_err(|err| format!("打开 SQLite 失败：{err}"))?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(|err| format!("启用 WAL 失败：{err}"))?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(|err| format!("启用外键失败：{err}"))?;
    Ok(connection)
}
