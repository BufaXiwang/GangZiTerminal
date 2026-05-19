//! search_news——本地资讯库 FTS5 全文搜索（标题/摘要/来源）。
//!
//! 数据范围：最近 30 天，多源（NewsNow / RSS / 6 个站点抽取器）汇总。
//! 索引：trigram tokenizer，对中文按 3 字符窗口建索引——「光模块」「北向资金」
//! 这种短词命中精确，毫秒级。FTS 失败时自动回退到 LIKE 子串匹配。
//!
//! 远端 web 搜索（Anthropic web_search / OpenAI Responses）是 server-side
//! 工具，由 provider 自己注入，不在本文件。

use crate::pipeline::agent::tools::{err_text, ok_json, Tool, ToolContext};
use crate::domain::agent::types::ToolResultContent;
use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::AppHandle;

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
        "本地资讯 FTS5 搜索（30 天内多源）。先调本工具；找不到再 web_search。\
        例：query='光模块 北向' / '600519 分红'。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "关键词，如 '光模块' 或 '600519'" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 30, "default": 5 }
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
            .unwrap_or(5)
            .clamp(1, 30);
        let app = self.app.clone();
        let result = tokio::task::spawn_blocking(move || query_news(app, query, limit))
            .await
            .map_err(|e| format!("search_news 任务异常：{e}"));
        match result {
            Ok(Ok(v)) => (ok_json(v), false),
            Ok(Err(msg)) => err_text(msg),
            Err(msg) => err_text(msg),
        }
    }
}

fn query_news(app: AppHandle, query: String, limit: u64) -> Result<Value, String> {
    let rows = crate::infrastructure::news::repository::search_news_items(
        app,
        query.clone(),
        Some(limit as i64),
    )?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|item| {
            json!({
                "id": item.id,
                "title": item.title,
                "source": item.source,
                "published": item.published,
                "summary": item.summary,
                "link": item.link,
            })
        })
        .collect();
    Ok(json!({"query": query, "count": items.len(), "items": items}))
}
