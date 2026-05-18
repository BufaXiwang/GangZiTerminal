//! SQLite 通用工具——给各 BC repository 共用。
//!
//! - `now()`：RFC3339 时间戳字符串
//! - `required_json_string` / `json_string`：JSON Value pointer 取字段（兜底 i64/u64 转 String）
//! - `list_json_payloads`：通用 `select payload_json ... limit ?1` 解析

use rusqlite::{params, Connection};
use serde_json::Value;

pub fn list_json_payloads(
    connection: &Connection,
    sql: &str,
    limit: i64,
    context: &str,
) -> Result<Vec<Value>, String> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|err| format!("{context}：{err}"))?;
    let payloads = statement
        .query_map(params![limit], |row| row.get::<_, String>(0))
        .map_err(|err| format!("{context}：{err}"))?
        .map(|raw| {
            raw.map_err(|err| format!("{context}：{err}"))
                .and_then(|text| {
                    serde_json::from_str::<Value>(&text)
                        .map_err(|err| format!("{context} JSON解析失败：{err}"))
                })
        })
        .collect();
    payloads
}

pub fn required_json_string(value: &Value, pointer: &str, message: &str) -> Result<String, String> {
    json_string(value, pointer).ok_or_else(|| message.to_string())
}

pub fn json_string(value: &Value, pointer: &str) -> Option<String> {
    value.pointer(pointer).and_then(|field| {
        field
            .as_str()
            .map(str::to_string)
            .or_else(|| field.as_i64().map(|number| number.to_string()))
            .or_else(|| field.as_u64().map(|number| number.to_string()))
    })
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
