//! SQLite 连接 + 数据库文件路径 + DatabaseInfo DTO。

use rusqlite::Connection;
use serde::Serialize;
use std::fs;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

pub const SCHEMA_VERSION: i64 = 5;

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
    // Schema v2 重构：如果存在老 DB（v1 schema）就备份后建新的——不背历史包袱
    backup_legacy_database_if_needed(&path)?;
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

/// Agent 重构 v2 一次性切换：
/// 若现存 DB 的 schema_meta.version < SCHEMA_VERSION，将整个文件 rename 成
/// `gangzi-terminal.sqlite3.legacy-{unix-ts}`，让 `Connection::open` 建一个空 DB。
///
/// 这取代了过去的 in-place `upgrade_*` / `drop_legacy_*` 套路——SQL schema 重新构建
/// 干净从 v2 开始；旧数据备份在原目录可手动检查。
fn backup_legacy_database_if_needed(path: &PathBuf) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    // 打开看 schema_meta.version
    let conn = match Connection::open(path) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "无法打开旧 DB，跳过备份判断");
            return Ok(());
        }
    };
    let version: Option<i64> = conn
        .query_row(
            "select version from schema_meta where id = 1",
            [],
            |row| row.get(0),
        )
        .ok();
    drop(conn);
    let needs_backup = match version {
        None => false, // 没有 schema_meta 表——空 DB，让 migrate 建即可
        Some(v) if v >= SCHEMA_VERSION => false,
        Some(_) => true,
    };
    if !needs_backup {
        return Ok(());
    }
    let ts = chrono::Utc::now().timestamp();
    let bak = path.with_file_name(format!(
        "{}.legacy-{ts}",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("gangzi-terminal.sqlite3")
    ));
    tracing::warn!(
        from = %path.display(),
        to = %bak.display(),
        from_version = ?version,
        to_version = SCHEMA_VERSION,
        "Schema 升级到 v2：备份旧 DB 后从空白重建"
    );
    fs::rename(path, &bak).map_err(|err| format!("备份旧 SQLite 文件失败：{err}"))?;
    // 同时把 WAL / SHM 一起带走，避免新 DB 复用残留
    for ext in &["-wal", "-shm"] {
        let aux = path.with_file_name(format!(
            "{}{ext}",
            path.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("gangzi-terminal.sqlite3")
        ));
        if aux.exists() {
            let _ = fs::rename(&aux, aux.with_extension(format!("legacy-{ts}{ext}")));
        }
    }
    Ok(())
}
