//! app_state 表 KV CRUD——基础读/写/删，单 row 一对 key-value JSON。
//!
//! 写入 conflict 行为：upsert（覆盖 value_json + updated_at）。
//! 读取：缺失返 Ok(None)；JSON 解析失败返 Err。

use crate::infrastructure::db::{migrate, now, open_database};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use tauri::AppHandle;

pub fn save_app_state_value(app: &AppHandle, key: &str, value: &Value) -> Result<(), String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    let now = now();
    connection
        .execute(
            "insert into app_state (key, value_json, updated_at)
             values (?1, ?2, ?3)
             on conflict(key) do update set value_json = excluded.value_json, updated_at = excluded.updated_at",
            params![key, value.to_string(), now],
        )
        .map_err(|err| format!("保存本地状态失败：{err}"))?;
    Ok(())
}

pub fn delete_app_state_value(app: &AppHandle, key: &str) -> Result<(), String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    connection
        .execute("delete from app_state where key = ?1", params![key])
        .map_err(|err| format!("删除本地状态失败：{err}"))?;
    Ok(())
}

pub fn load_app_state_value(app: &AppHandle, key: &str) -> Result<Option<Value>, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    let raw = connection
        .query_row(
            "select value_json from app_state where key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|err| format!("读取本地状态失败：{err}"))?;

    raw.map(|text| {
        serde_json::from_str(&text).map_err(|err| format!("本地状态 JSON 解析失败：{err}"))
    })
    .transpose()
}
