//! News 批量分析触发 loop——把 pending news 攒批喊 agent。
//!
//! 触发条件（OR）：
//! 1. **Timer**：每 N 分钟（默认 15）兜底跑一次
//! 2. **Buffer overflow**：pending 数 ≥ M（默认 20）立即触发（由 refresh tick 末尾顺手 check）
//!
//! 每次触发：
//!   1. watchdog 回收超时 processing 孤儿
//!   2. claim_batch(M) 原子取走
//!   3. emit "news-batch-ready" 事件——adapter 层 listener 收到后启动 agent run
//!
//! 注：pipeline 不允许直接 import adapters，所以走 Tauri Event 解耦。
//! agent 的 news_review pipeline 由 adapters/news_batch_listener.rs 触发。

use crate::infrastructure::news::batch;
use serde_json::json;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// 一次取的最大 news 数——M
pub const DEFAULT_BATCH_SIZE: usize = 20;
/// 定时触发间隔（秒）——N
pub const DEFAULT_INTERVAL_SECS: u64 = 15 * 60;
/// Watchdog 超时回收阈值（分钟）
pub const STALE_CUTOFF_MINUTES: i64 = 30;

const KEY_BATCH_SIZE: &str = "gangzi-terminal.news.batch-size";
const KEY_BATCH_INTERVAL: &str = "gangzi-terminal.news.batch-interval-secs";

/// 主循环——scheduler 启动时 spawn 一次。
pub async fn news_batch_loop(app: AppHandle) {
    // 启动延迟——让 news_refresh_loop 先 hydrate 几条
    tokio::time::sleep(Duration::from_secs(45)).await;

    loop {
        run_tick(&app, TriggerReason::Timer).await;
        let interval = read_interval(&app);
        tokio::time::sleep(interval).await;
    }
}

/// 由 refresh tick 末尾调——若 pending ≥ M 立即触发，否则等 timer。
pub async fn check_buffer_overflow(app: &AppHandle) {
    let m = read_batch_size(app);
    let pending = batch::count_pending(app).unwrap_or(0) as usize;
    if pending >= m {
        tracing::info!(pending, threshold = m, "buffer overflow → 立即触发 news batch");
        run_tick(app, TriggerReason::BufferOverflow).await;
    }
}

#[derive(Debug, Clone, Copy)]
pub enum TriggerReason {
    Timer,
    BufferOverflow,
}

impl TriggerReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Timer => "timer",
            Self::BufferOverflow => "buffer_overflow",
        }
    }
}

async fn run_tick(app: &AppHandle, reason: TriggerReason) {
    // 1. watchdog 回收超时 processing
    if let Err(e) = batch::reclaim_stale_processing(app, STALE_CUTOFF_MINUTES) {
        tracing::warn!(error = %e, "watchdog reclaim 失败，跳过本轮");
    }

    let m = read_batch_size(app);
    // 2. claim batch
    let ids = match batch::claim_batch(app, m) {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %e, "claim_batch 失败");
            return;
        }
    };
    if ids.is_empty() {
        tracing::debug!(reason = reason.as_str(), "news batch 无 pending，跳过");
        return;
    }

    // 3. emit 事件——adapter listener 启动 agent run
    let remaining = batch::count_pending(app).unwrap_or(0);
    let batch_id = uuid::Uuid::new_v4().to_string();
    let news_ids: Vec<String> = ids.iter().map(|i| i.as_str().to_string()).collect();
    let captured_at = chrono::Utc::now().timestamp_millis();
    tracing::info!(
        batch_id = %batch_id,
        reason = reason.as_str(),
        count = news_ids.len(),
        remaining,
        "news batch ready → emit"
    );
    let payload = json!({
        "batchId": batch_id,
        "newsIds": news_ids,
        "queuedRemaining": remaining,
        "triggerReason": reason.as_str(),
        "capturedAt": captured_at,
    });
    let _ = app.emit("news-batch-ready", payload);
}

fn read_batch_size(app: &AppHandle) -> usize {
    crate::infrastructure::app_state::load_app_state_value(app, KEY_BATCH_SIZE)
        .ok()
        .flatten()
        .and_then(|v| v.as_u64())
        .map(|n| (n as usize).clamp(1, 100))
        .unwrap_or(DEFAULT_BATCH_SIZE)
}

fn read_interval(app: &AppHandle) -> Duration {
    let secs = crate::infrastructure::app_state::load_app_state_value(app, KEY_BATCH_INTERVAL)
        .ok()
        .flatten()
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_INTERVAL_SECS);
    Duration::from_secs(secs.clamp(60, 3600))
}
