//! Agent 对 News 的批量分析编排——把 pending news 攒批喊 agent review。
//!
//! News BC 只提供"拉取 + 存储 + 查询"。本 pipeline 是 Agent 域的事：
//! 1. 监听 News BC 发的 `news-refreshed` event 做 buffer overflow check
//! 2. adapter 层定时 N 分钟 tick 做兜底
//!
//! 内部用 `tokio::spawn` 模型——adapter 调本模块 `run_once` await 到完成，
//! 不再走 event 解耦（同模块内无必要）。
//!
//! 并发安全 = `REVIEW_IN_FLIGHT` 进程级 AtomicBool：
//! - run_once 一开始 try_mark；上一轮没结束就直接跳过
//! - RAII Drop guard 保证成功 / 失败 / panic 都释放锁
//!
//! 失败处理：agent run Err → revert news 到 pending（等下次重试）；不写 failed
//! 是为了防 provider 瞬时故障导致永久漏分析。

use crate::domain::news::NewsId;
use crate::infrastructure::agent::news_analysis_repo;
use crate::pipeline::agent::news_review;
use crate::pipeline::agent::tools::ToolRegistry;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::AppHandle;

/// 一次取的最大 news 数——M
pub const DEFAULT_BATCH_SIZE: usize = 20;
/// 定时触发间隔（秒）——N
pub const DEFAULT_INTERVAL_SECS: u64 = 15 * 60;
/// Watchdog 超时回收阈值（分钟）
pub const STALE_CUTOFF_MINUTES: i64 = 30;

const KEY_BATCH_SIZE: &str = "gangzi-terminal.news.batch-size";
const KEY_BATCH_INTERVAL: &str = "gangzi-terminal.news.batch-interval-secs";

/// 全局 in-flight 锁——同一时间只允许 1 个 news_review 在跑。
/// 进程级 AtomicBool，进程重启自动清空；run 卡死 30min 由 news watchdog 回收 processing news。
static REVIEW_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// RAII guard——drop 时释放 REVIEW_IN_FLIGHT。
/// 保证 run_once 内部不管哪条 return 路径都释放（含 panic 经 unwind 时）。
struct ReviewDoneGuard;
impl Drop for ReviewDoneGuard {
    fn drop(&mut self) {
        REVIEW_IN_FLIGHT.store(false, Ordering::SeqCst);
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

/// adapter 定时 / event handler 调本函数——await 直到 review 跑完。
///
/// 行为：
/// 1. watchdog 回收 30min 前的 processing 孤儿
/// 2. try acquire in-flight 锁（上一轮没结束就 skip 本轮）
/// 3. claim_batch(M) 原子取走
/// 4. await news_review::run
/// 5. 按结果 mark_consumed / revert_processing_to_pending
///
/// 锁通过 RAII guard 在函数 return 时自动释放。
pub async fn run_once(
    app: AppHandle,
    registry: Arc<ToolRegistry>,
    reason: TriggerReason,
) -> Result<(), String> {
    // 1. watchdog
    if let Err(e) = news_analysis_repo::reclaim_stale_processing(&app, STALE_CUTOFF_MINUTES) {
        tracing::warn!(error = %e, "watchdog reclaim 失败，继续本轮");
    }

    // 2. in-flight gate
    if REVIEW_IN_FLIGHT
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        tracing::debug!(reason = reason.as_str(), "上一轮 news_review 仍在进行，跳过本轮");
        return Ok(());
    }
    let _guard = ReviewDoneGuard;

    // 3. claim
    let m = read_batch_size(&app);
    let ids = match news_analysis_repo::claim_batch(&app, m) {
        Ok(ids) => ids,
        Err(e) => return Err(format!("claim_batch 失败：{e}")),
    };
    if ids.is_empty() {
        tracing::debug!(reason = reason.as_str(), "news batch 无 pending，跳过");
        return Ok(());
    }

    // 4. 跑 agent review
    let batch_id = uuid::Uuid::new_v4().to_string();
    let remaining = news_analysis_repo::count_pending(&app).unwrap_or(0);
    let count = ids.len();
    tracing::info!(
        batch_id = %batch_id,
        reason = reason.as_str(),
        count,
        remaining,
        "news batch claim → 跑 news_review"
    );

    let news_ids_for_run: Vec<NewsId> = ids.clone();
    match news_review::run(
        app.clone(),
        registry,
        batch_id.clone(),
        news_ids_for_run,
        remaining,
        reason.as_str().to_string(),
    )
    .await
    {
        Ok(run_id) => {
            tracing::info!(batch_id = %batch_id, run_id = %run_id, count, "news_review 完成 → mark_consumed");
            if let Err(e) = news_analysis_repo::mark_consumed(&app, &ids) {
                tracing::warn!(error = %e, batch = %batch_id, "mark_consumed 失败");
            }
            Ok(())
        }
        Err(e) => {
            // 系统级失败 → revert pending，让下次重试
            tracing::warn!(batch_id = %batch_id, error = %e, count, "news_review 失败 → revert pending");
            if let Err(e2) = news_analysis_repo::revert_processing_to_pending(&app, &ids) {
                tracing::warn!(error = %e2, batch = %batch_id, "revert_pending 失败");
            }
            Err(e)
        }
    }
    // _guard drop here → REVIEW_IN_FLIGHT cleared
}

/// 拉用户配置——M（batch size），失败回默认。
pub fn read_batch_size(app: &AppHandle) -> usize {
    crate::infrastructure::app_state::load_app_state_value(app, KEY_BATCH_SIZE)
        .ok()
        .flatten()
        .and_then(|v| v.as_u64())
        .map(|n| (n as usize).clamp(1, 100))
        .unwrap_or(DEFAULT_BATCH_SIZE)
}

/// 拉用户配置——N（定时间隔秒），失败回默认。
pub fn read_interval_secs(app: &AppHandle) -> u64 {
    crate::infrastructure::app_state::load_app_state_value(app, KEY_BATCH_INTERVAL)
        .ok()
        .flatten()
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_INTERVAL_SECS)
        .clamp(60, 3600)
}

/// adapter 收到 news-refreshed event 后调——若 pending ≥ M 立即触发一轮 run_once。
pub async fn maybe_buffer_overflow(app: &AppHandle, registry: Arc<ToolRegistry>) {
    let m = read_batch_size(app);
    let pending = news_analysis_repo::count_pending(app).unwrap_or(0) as usize;
    if pending >= m {
        tracing::info!(pending, threshold = m, "buffer overflow → 触发 news batch");
        let _ = run_once(app.clone(), registry, TriggerReason::BufferOverflow).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_flight_lock_blocks_second_acquire() {
        // 静态：测试间共享，先清干净
        REVIEW_IN_FLIGHT.store(false, Ordering::SeqCst);
        assert!(REVIEW_IN_FLIGHT
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok());
        assert!(REVIEW_IN_FLIGHT
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err());
        REVIEW_IN_FLIGHT.store(false, Ordering::SeqCst);
    }
}
