//! get_position——查模拟持仓全档案 + 完整事件链。
//!
//! SQLite 查询是同步阻塞的，通过 `tokio::task::spawn_blocking` 把它挪到
//! blocking 线程池，避免占住 agent loop 所在的 async worker。

use crate::agent::tools::{err_text, ok_json, Tool, ToolContext};
use crate::agent::types::ToolResultContent;
use async_trait::async_trait;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

pub struct GetPositionTool {
    app: AppHandle,
}

impl GetPositionTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for GetPositionTool {
    fn name(&self) -> &'static str {
        "get_position"
    }

    fn description(&self) -> &'static str {
        "查询模拟持仓的全档案——基础信息（开仓价、止损、止盈、入场逻辑）+ 完整事件链\
        （opened/reviewed/adjusted/closed 等）。复盘和调仓判断时调用。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "positionId": { "type": "string", "description": "持仓 id（UUID）" }
            },
            "required": ["positionId"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let position_id = match input.get("positionId").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return err_text("missing positionId"),
        };
        let db_path = match resolve_db_path(&self.app) {
            Ok(p) => p,
            Err(e) => return err_text(e),
        };
        let result = tokio::task::spawn_blocking(move || query_position(&db_path, &position_id))
            .await
            .map_err(|e| format!("get_position 任务异常：{e}"));
        match result {
            Ok(Ok(value)) => (ok_json(value), false),
            Ok(Err(msg)) => err_text(msg),
            Err(msg) => err_text(msg),
        }
    }
}

fn resolve_db_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("拿不到 app_data_dir：{err}"))?;
    Ok(dir.join("gangzi-terminal.sqlite3"))
}

fn query_position(db_path: &std::path::Path, position_id: &str) -> Result<Value, String> {
    let conn = Connection::open(db_path).map_err(|err| format!("打开 SQLite 失败：{err}"))?;
    let payload: String = conn
        .query_row(
            "select payload_json from simulated_positions where id = ?1",
            params![position_id],
            |row| row.get(0),
        )
        .map_err(|err| format!("未找到持仓 {position_id}：{err}"))?;
    let position: Value =
        serde_json::from_str(&payload).map_err(|err| format!("持仓 JSON 解析失败：{err}"))?;
    let mut stmt = conn
        .prepare(
            "select payload_json from position_events
             where position_id = ?1 order by occurred_at asc limit 50",
        )
        .map_err(|err| format!("准备事件查询失败：{err}"))?;
    let events: Vec<Value> = stmt
        .query_map(params![position_id], |row| row.get::<_, String>(0))
        .map_err(|err| format!("事件查询失败：{err}"))?
        .filter_map(|raw| raw.ok())
        .filter_map(|text| serde_json::from_str::<Value>(&text).ok())
        .collect();
    Ok(json!({ "position": position, "events": events }))
}
