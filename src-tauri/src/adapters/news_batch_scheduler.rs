//! Agent 对 News 的批量分析调度——adapter 层入口。
//!
//! 放在 adapters/ 因为它要 import `agent_tools::build_chat_registry` 构造
//! ToolRegistry 注入 pipeline（pipeline 不允许 use adapters）。
//!
//! 两条触发路径，都调 `pipeline::agent::news_batch::run_once` await 到完成：
//! 1. **Timer**：每 N 分钟（默认 15，可在 SettingsPage 调）兜底 tick
//! 2. **Buffer overflow**：监听 News BC 发的 `news-refreshed` event，pending ≥ M
//!    时立即触发——News BC 不感知本 listener，符合"News 只提供拉取能力"边界
//!
//! In-flight 锁 + watchdog 在 pipeline/agent/news_batch 内部维护，本文件只管"调度"。

use crate::adapters::agent_tools::build_chat_registry;
use crate::pipeline::agent::news_batch;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Listener};

pub fn spawn(app: AppHandle) {
    // 1. Timer loop
    tauri::async_runtime::spawn(timer_loop(app.clone()));
    // 2. news-refreshed 事件监听 → buffer overflow check
    spawn_news_refreshed_listener(app);
}

async fn timer_loop(app: AppHandle) {
    // 启动延迟——等 news_refresh 先拉几条
    tokio::time::sleep(Duration::from_secs(45)).await;
    loop {
        let registry = Arc::new(build_chat_registry(&app));
        if let Err(e) =
            news_batch::run_once(app.clone(), registry, news_batch::TriggerReason::Timer).await
        {
            tracing::warn!(error = %e, "news_batch timer tick 失败");
        }
        let interval = Duration::from_secs(news_batch::read_interval_secs(&app));
        tokio::time::sleep(interval).await;
    }
}

fn spawn_news_refreshed_listener(app: AppHandle) {
    let app_for_handler = app.clone();
    app.listen("news-refreshed", move |_event| {
        let app_clone = app_for_handler.clone();
        tauri::async_runtime::spawn(async move {
            let registry = Arc::new(build_chat_registry(&app_clone));
            news_batch::maybe_buffer_overflow(&app_clone, registry).await;
        });
    });
}
