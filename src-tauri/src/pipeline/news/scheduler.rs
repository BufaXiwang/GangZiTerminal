//! News 后台 refresh 调度。
//!
//! Spec: docs/design/news-module.md §5（默认 60s，触发节奏由模块外运行时配置）
//!
//! 当前阶段：tick 在固定间隔触发 `refresh_news`，并通过回调把事件 payload 推给 adapters
//! 层（adapters 用 Tauri `emit` 推送给前端）。
//!
//! `news-refreshed` 只在 `savedCount > 0 || articleUpdatedCount > 0` 时发布（spec §5）。

use crate::domain::news::events::NewsRefreshedPayload;
use crate::domain::news::types::{RefreshNewsRequest, RefreshNewsResponse};
use crate::pipeline::news::service::NewsService;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

pub const NEWS_REFRESH_INTERVAL_SECS: u64 = 60;

/// Adapters / 上层注册的事件回调。`emit` 同步触发，避免运行时跨 BC 依赖。
pub type EventSink = Arc<dyn Fn(NewsRefreshedPayload) + Send + Sync + 'static>;

/// 控制后台 scheduler 生命周期的 handle。drop 时取消任务。
pub struct NewsSchedulerHandle {
    _join: JoinHandle<()>,
    _stop: mpsc::Sender<()>,
}

/// Spawn `news-refreshed` 后台调度任务。
///
/// `interval`: tick 间隔（spec §5 默认 60s）。
/// `sink`: 当 refresh 产生 `savedCount > 0 || articleUpdatedCount > 0` 时由 scheduler 调用。
pub fn spawn_news_refresh_scheduler(
    service: Arc<NewsService>,
    interval: Duration,
    sink: EventSink,
) -> NewsSchedulerHandle {
    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
    let join = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // 第一 tick 立即返回，跳过一次避免启动瞬间立刻刷新
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = stop_rx.recv() => break,
                _ = ticker.tick() => {
                    let svc = Arc::clone(&service);
                    let s = sink.clone();
                    match svc.refresh_news(RefreshNewsRequest::default()).await {
                        RefreshNewsResponse::Ok(ok) => {
                            let payload = ok.result;
                            if payload.saved_count > 0 || payload.article_updated_count > 0 {
                                s(payload);
                            }
                        }
                        RefreshNewsResponse::Err(err) => {
                            warn!(
                                target: "news.scheduler",
                                code = ?err.error.code,
                                message = ?err.error.message,
                                "scheduled news refresh rejected",
                            );
                        }
                    }
                }
            }
        }
    });
    NewsSchedulerHandle {
        _join: join,
        _stop: stop_tx,
    }
}
