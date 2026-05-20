//! News 批量分析触发 loop——把 pending news 攒批喊 agent。
//!
//! 触发条件（OR）：
//! 1. **Timer**：每 N 分钟（默认 15）兜底跑一次
//! 2. **Buffer overflow**：pending 数 ≥ M（默认 20）立即触发（由 refresh tick 末尾顺手 check）
//!
//! 每次触发：
//!   1. watchdog 回收超时 processing 孤儿
//!   2. **try_mark_review_starting**——若上一轮 agent run 还没结束就 skip 本轮
//!      （防止 agent 慢 / 多 batch 堆积导致 processing 队列膨胀）
//!   3. claim_batch(M) 原子取走
//!   4. emit "news-batch-ready"——adapter listener 启动 agent run，**run 完
//!      负责调用 `mark_review_done()`** 释放 in-flight 锁
//!
//! emit 失败 / 无 news 可取时立即调 `mark_review_done()` 回收锁。
//!
//! 注：pipeline 不允许直接 import adapters，所以走 Tauri Event 解耦。

use crate::infrastructure::news::batch;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// 一次取的最大 news 数——M
pub const DEFAULT_BATCH_SIZE: usize = 20;
/// 定时触发间隔（秒）——N
pub const DEFAULT_INTERVAL_SECS: u64 = 15 * 60;
/// Watchdog 超时回收阈值（分钟）——processing 超过此值视为孤儿
pub const STALE_CUTOFF_MINUTES: i64 = 30;

const KEY_BATCH_SIZE: &str = "gangzi-terminal.news.batch-size";
const KEY_BATCH_INTERVAL: &str = "gangzi-terminal.news.batch-interval-secs";

/// 全局 in-flight 锁——同一时间只允许 1 个 news_review 在跑（含已发 event 还没 run 完的）。
/// 进程级 AtomicBool，进程重启自动清空；run 卡死 30min 由 news watchdog 回收 processing news，
/// listener Drop guard 同时释放此锁（见 adapters/news_batch_listener.rs）。
static REVIEW_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// 试 acquire 锁。`true` = 拿到，调用方负责后续 `mark_review_done`；
/// `false` = 已有 run 在跑，本轮 skip。
pub fn try_mark_review_starting() -> bool {
    REVIEW_IN_FLIGHT
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

/// release 锁。listener run 结束（成功/失败/panic via RAII）后调。
pub fn mark_review_done() {
    REVIEW_IN_FLIGHT.store(false, Ordering::SeqCst);
}

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

    // 2. in-flight gate：上一轮 agent run 没结束就跳过——避免 processing 堆积
    if !try_mark_review_starting() {
        tracing::debug!(
            reason = reason.as_str(),
            "上一轮 news_review 仍在进行，跳过本轮 claim"
        );
        return;
    }
    // 从这里开始，本轮持有 REVIEW_IN_FLIGHT。任何 early return 必须 mark_review_done()。

    let m = read_batch_size(app);
    let ids = match batch::claim_batch(app, m) {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %e, "claim_batch 失败");
            mark_review_done();
            return;
        }
    };
    if ids.is_empty() {
        tracing::debug!(reason = reason.as_str(), "news batch 无 pending，跳过");
        mark_review_done();
        return;
    }

    // 3. emit 事件——adapter listener 启动 agent run，**listener Drop guard 释放锁**
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
        "newsIds": news_ids.clone(),
        "queuedRemaining": remaining,
        "triggerReason": reason.as_str(),
        "capturedAt": captured_at,
    });
    if let Err(e) = app.emit("news-batch-ready", payload) {
        // emit 失败极罕见——若发生，本轮 listener 不会来，得自己释放锁 + revert 已 claim 的 news
        tracing::warn!(error = %e, batch_id = %batch_id, "emit news-batch-ready 失败 → 释放锁 + revert news");
        let id_vec: Vec<crate::domain::news::NewsId> = news_ids
            .iter()
            .map(|s| crate::domain::news::NewsId::new(s.clone()))
            .collect();
        let _ = batch::revert_processing_to_pending(app, &id_vec);
        mark_review_done();
    }
    // 正常路径：listener 接到 event，run 完后自己 mark_review_done。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_flight_gate_blocks_second_acquire() {
        // 重置——这是个 process-static，测试间共享，先把它清掉
        mark_review_done();
        assert!(try_mark_review_starting(), "首次 acquire 必须成功");
        assert!(
            !try_mark_review_starting(),
            "已 in_flight 时第二次 acquire 必须失败"
        );
        mark_review_done();
        assert!(try_mark_review_starting(), "release 后再 acquire 应成功");
        mark_review_done();
    }
}
