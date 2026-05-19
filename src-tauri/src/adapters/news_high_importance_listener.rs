//! 监听 `news-high-importance-detected` 事件——pipeline::news::refresh 在
//! NewsImportance::High 资讯入库且涉及自选股时 emit 该事件。
//!
//! 本 listener 在 adapter 层是因为它要 import `agent_tools::build_chat_registry`
//! 构造 ToolRegistry 注入 pipeline::agent::scan::run_mini_scan（pipeline 不允许 import adapter）。

use crate::adapters::agent_tools::build_chat_registry;
use crate::domain::shared::StockCode;
use crate::pipeline::agent::scan::run_mini_scan;
use serde::Deserialize;
use std::sync::Arc;
use tauri::{AppHandle, Listener};

#[derive(Debug, Deserialize)]
struct Payload {
    code: String,
}

pub fn spawn(app: AppHandle) {
    let app_for_handler = app.clone();
    app.listen("news-high-importance-detected", move |event| {
        let raw = event.payload();
        let parsed: Option<Payload> = serde_json::from_str(raw).ok();
        let Some(payload) = parsed else {
            tracing::warn!(payload = raw, "news-high-importance-detected payload 解析失败");
            return;
        };
        let Ok(code) = StockCode::new(&payload.code) else {
            tracing::warn!(code = payload.code, "非法 code，跳过");
            return;
        };
        let app_clone = app_for_handler.clone();
        tauri::async_runtime::spawn(async move {
            let registry = Arc::new(build_chat_registry(&app_clone));
            // 这里 signals 列表传一条 NewsCatalystMatched 占位（具体 kind/importance 由 scan 内部
            // 通过 news_tag_repo 拉关联资讯再补；这里只触发，让 scan 自己 enrich context）
            let tick_id = format!("news-high-{}", uuid::Uuid::new_v4());
            match run_mini_scan(
                app_clone,
                registry,
                code.clone(),
                vec![], // signals 由 scan_one_stock 自己拉关联 news 重新检测
                "news_high_importance",
                &tick_id,
                "news-high-importance",
            )
            .await
            {
                Ok(run_id) => tracing::info!(code = %code, run_id = %run_id, "News High → mini-scan 完成"),
                Err(e) => tracing::warn!(code = %code, error = %e, "News High → mini-scan 失败"),
            }
        });
    });
}
