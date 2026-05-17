//! search_news——已落库资讯按 LIKE 模糊搜索（标题/摘要/来源）。
//!
//! 仅查询本地缓存。远端搜索（Eastmoney 公告、CLS 电报、Anthropic web_search）
//! 是另一组工具，本文件不涉及。

use crate::infrastructure::agent::tools::{err_text, ok_json, Tool, ToolContext};
use crate::domain::agent::types::ToolResultContent;
use async_trait::async_trait;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

pub struct SearchNewsTool {
    app: AppHandle,
}

impl SearchNewsTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for SearchNewsTool {
    fn name(&self) -> &'static str {
        "search_news"
    }

    fn description(&self) -> &'static str {
        "已落库资讯全文搜索（按 LIKE 匹配标题/摘要/来源）。limit 默认 20，最大 50。\
        按发布时间倒序。回查历史背景、印证当前事件时调用。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "关键词，如 '光模块' 或 '600519'" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 20 }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let query = match input.get("query").and_then(Value::as_str).map(str::trim) {
            Some(q) if !q.is_empty() => q.to_string(),
            _ => return err_text("query 不能为空"),
        };
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .clamp(1, 50);
        let db_path = match self
            .app
            .path()
            .app_data_dir()
            .map_err(|e| format!("拿不到 app_data_dir：{e}"))
        {
            Ok(d) => d.join("gangzi-terminal.sqlite3"),
            Err(e) => return err_text(e),
        };
        let result = tokio::task::spawn_blocking(move || query_news(&db_path, &query, limit))
            .await
            .map_err(|e| format!("search_news 任务异常：{e}"));
        match result {
            Ok(Ok(v)) => (ok_json(v), false),
            Ok(Err(msg)) => err_text(msg),
            Err(msg) => err_text(msg),
        }
    }
}

fn query_news(db_path: &std::path::Path, query: &str, limit: u64) -> Result<Value, String> {
    let conn = Connection::open(db_path).map_err(|err| format!("打开 SQLite 失败：{err}"))?;
    let pattern = format!("%{query}%");
    let mut stmt = conn
        .prepare(
            "select payload_json from news_items
             where payload_json like ?1
             order by coalesce(published, created_at) desc
             limit ?2",
        )
        .map_err(|err| format!("prepare 失败：{err}"))?;
    let items: Vec<Value> = stmt
        .query_map(params![pattern, limit as i64], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|err| format!("查询失败：{err}"))?
        .filter_map(|raw| raw.ok())
        .filter_map(|text| serde_json::from_str::<Value>(&text).ok())
        // 只保留对 Agent 真正有用的字段，减少 payload 大小
        .map(|v| {
            json!({
                "id":        v.get("id"),
                "title":     v.get("title"),
                "source":    v.get("source"),
                "published": v.get("published"),
                "summary":   v.get("summary"),
                "link":      v.get("link"),
            })
        })
        .collect();
    Ok(json!({"query": query, "count": items.len(), "items": items}))
}
