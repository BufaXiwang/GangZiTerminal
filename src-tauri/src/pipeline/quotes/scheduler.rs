//! Quotes 后台调度。
//!
//! Spec: docs/design/quotes-module.md §5（实时行情 15s 关注 / 60s universe；收盘 + daily_basic +
//! kline + 公司事件 + universe 启动 / 每日 08:30）
//!
//! 当前实现：单 tick 任务，每 60s 触发一次 universe quote refresh；如果在交易时段则刷新
//! 核心指数 + universe。简化版本，更多频率档由 main agent 后续接入 agent runtime。

use crate::domain::quotes::{RefreshPurpose, RefreshScope};
use crate::pipeline::quotes::service::{QuotesService, RefreshMarketQuotesRequest};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

pub const QUOTES_REFRESH_INTERVAL_SECS: u64 = 60;

pub struct QuotesSchedulerHandle {
    _join: JoinHandle<()>,
    _stop: mpsc::Sender<()>,
}

pub fn spawn_quotes_scheduler(
    service: Arc<QuotesService>,
    interval: Duration,
) -> QuotesSchedulerHandle {
    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
    let join = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = stop_rx.recv() => break,
                _ = ticker.tick() => {
                    let svc = Arc::clone(&service);
                    // 默认：刷新核心指数（subscribed scope，调用方未来传入合并集）。
                    let core = svc.core_indexes();
                    let req = RefreshMarketQuotesRequest {
                        scope: RefreshScope::Subscribed,
                        purpose: RefreshPurpose::Intraday,
                        ts_codes: Some(core),
                        trade_date: None,
                    };
                    if let Err(e) = svc.refresh_market_quotes(req).await {
                        warn!(target: "quotes.scheduler", error = ?e, "scheduled quote refresh failed");
                    }
                }
            }
        }
    });
    QuotesSchedulerHandle {
        _join: join,
        _stop: stop_tx,
    }
}
