//! 持仓 / 自选股相关的只读 + 轻量写工具。
//!
//! - `get_position`：查模拟持仓全档案 + 完整事件链
//! - `add_to_watchlist`：把代码加入自选股（agent 看完 news 后觉得"该跟踪一下"用）

use crate::domain::agent::types::ToolResultContent;
use crate::domain::shared::StockCode;
use crate::pipeline::agent::tools::{err_text, ok_json, Tool, ToolContext};
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
        "查模拟持仓全档案（基础信息 + 完整事件链）。复盘 / 调仓判断时调。"
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

// ============================================================================
// add_to_watchlist
// ============================================================================

pub struct AddToWatchlistTool {
    app: AppHandle,
}

impl AddToWatchlistTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for AddToWatchlistTool {
    fn name(&self) -> &'static str {
        "add_to_watchlist"
    }

    fn description(&self) -> &'static str {
        "把一只 A 股加入自选股——agent 看新闻 / 扫盘后觉得值得跟踪但还没下注时调。\
        加入后 quotes refresh loop 会自动订阅其行情。reason 字段说明为什么值得跟踪，\
        供事后审计。重复添加幂等（不报错）。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "A 股 6 位代码或可解析的中文名" },
                "reason": { "type": "string", "description": "为什么加入自选——审计用，≤200 字" }
            },
            "required": ["code", "reason"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let raw = input
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if raw.is_empty() {
            return err_text("missing code");
        }
        let reason = input
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if reason.is_empty() {
            return err_text("missing reason（说明为什么加入自选）");
        }
        let stock = match crate::pipeline::stocks::resolve_stock(&self.app, raw).await {
            Ok(s) => s,
            Err(e) => return err_text(format!("code 解析失败：{e}")),
        };
        let code = match StockCode::new(&stock.code) {
            Ok(c) => c,
            Err(e) => return err_text(format!("非法 code {}: {e:?}", stock.code)),
        };
        let already = crate::infrastructure::account::watchlist::contains(&code);
        crate::infrastructure::account::watchlist::add(&self.app, code.clone());
        tracing::info!(
            code = %code.as_str(),
            already_in_list = already,
            reason = %reason,
            "agent add_to_watchlist"
        );
        (
            ok_json(json!({
                "ok": true,
                "code": code.as_str(),
                "name": stock.name,
                "already_in_list": already,
            })),
            false,
        )
    }
}

// ============================================================================
// get_position 内部
// ============================================================================

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
