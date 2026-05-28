//! Quotes 后台调度。
//!
//! Spec: docs/design/quotes-module.md §5 后台刷新。
//!
//! 调度档：
//! - 关注 quote 15s（subscribed_intraday）
//! - universe quote 60s（universe_intraday）
//! - 收盘快照 15:30 触发（close_snapshot）
//!   + 5min 重试，最多 6 次，直到当日完整完成。
//! - 16:00 K 线预热（universe daily K + qfq/hfq）
//! - 每日 09:00 daily_basic（上一交易日）
//! - 每日 09:15 公司事件（dividends + suspensions T-3..T+30）

use crate::domain::quotes::{
    core_indexes, RefreshDataScope, RefreshMarketQuotesScope, RefreshPurpose,
};
use crate::infrastructure::quotes::TradeCalendar;
use crate::pipeline::quotes::service::{QuotesService, RefreshMarketQuotesRequest};
use chrono::Timelike;
use chrono_tz::Asia::Shanghai;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

pub const QUOTES_REFRESH_INTERVAL_SECS: u64 = 60;
pub const QUOTES_SUBSCRIBED_INTERVAL_SECS: u64 = 15;

pub struct QuotesSchedulerHandle {
    _joins: Vec<JoinHandle<()>>,
    _stops: Vec<mpsc::Sender<()>>,
}

/// 单 tick scheduler — 兼容旧 API，等价于 universe 60s tick。
pub fn spawn_quotes_scheduler(
    service: Arc<QuotesService>,
    interval: Duration,
) -> QuotesSchedulerHandle {
    spawn_full_scheduler(
        service,
        QuotesSchedulerIntervals {
            universe_interval: interval,
            subscribed_interval: Duration::from_secs(QUOTES_SUBSCRIBED_INTERVAL_SECS),
            daily_tick_interval: Duration::from_secs(60),
        },
    )
}

#[derive(Debug, Clone)]
pub struct QuotesSchedulerIntervals {
    pub universe_interval: Duration,
    pub subscribed_interval: Duration,
    pub daily_tick_interval: Duration,
}

impl Default for QuotesSchedulerIntervals {
    fn default() -> Self {
        Self {
            universe_interval: Duration::from_secs(QUOTES_REFRESH_INTERVAL_SECS),
            subscribed_interval: Duration::from_secs(QUOTES_SUBSCRIBED_INTERVAL_SECS),
            daily_tick_interval: Duration::from_secs(60),
        }
    }
}

/// 启动 Quotes 完整 multi-tick scheduler。
///
/// Spec: quotes-module.md §5 后台刷新。
pub fn spawn_full_scheduler(
    service: Arc<QuotesService>,
    intervals: QuotesSchedulerIntervals,
) -> QuotesSchedulerHandle {
    let mut joins = Vec::new();
    let mut stops = Vec::new();

    // ----- subscribed 15s tick（关注 + 核心指数 quote refresh）
    {
        let svc = Arc::clone(&service);
        let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let interval = intervals.subscribed_interval;
        let h = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = stop_rx.recv() => break,
                    _ = ticker.tick() => {
                        // 当前未持有 Account 关注列表（spec §5 — 调用方传入合并集）；
                        // scheduler 内部至少用核心指数 + 任意 cache 中已有的 ts_code 作 subscribed scope。
                        let ctx = svc.market_time_now();
                        if !ctx.is_trading_time {
                            continue;
                        }
                        let mut codes = core_indexes();
                        // 把当前 cache 中已有的 ts_code 也合并进来作为 hot list。
                        let mut seen: std::collections::HashSet<_> =
                            codes.iter().cloned().collect();
                        for snap in svc.cache().snapshot_all() {
                            if seen.insert(snap.quote.ts_code.clone()) {
                                codes.push(snap.quote.ts_code);
                            }
                        }
                        let req = RefreshMarketQuotesRequest {
                            scope: RefreshMarketQuotesScope::Subscribed { ts_codes: codes },
                            purpose: RefreshPurpose::Intraday,
                            trade_date: None,
                        };
                        if let Err(e) = svc.refresh_market_quotes(req).await {
                            warn!(target: "quotes.scheduler.subscribed", error = ?e, "subscribed quote tick failed");
                        }
                    }
                }
            }
        });
        joins.push(h);
        stops.push(stop_tx);
    }

    // ----- universe 60s tick
    {
        let svc = Arc::clone(&service);
        let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let interval = intervals.universe_interval;
        let h = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = stop_rx.recv() => break,
                    _ = ticker.tick() => {
                        let ctx = svc.market_time_now();
                        if !ctx.is_trading_time {
                            continue;
                        }
                        let req = RefreshMarketQuotesRequest {
                            scope: RefreshMarketQuotesScope::Universe,
                            purpose: RefreshPurpose::Intraday,
                            trade_date: None,
                        };
                        if let Err(e) = svc.refresh_market_quotes(req).await {
                            warn!(target: "quotes.scheduler.universe", error = ?e, "universe tick failed");
                        }
                    }
                }
            }
        });
        joins.push(h);
        stops.push(stop_tx);
    }

    // ----- 日常时间窗口 tick（每 60s 检查 15:30 close / 16:00 K / 09:00 daily_basic / 09:15 events）
    {
        let svc = Arc::clone(&service);
        let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let interval = intervals.daily_tick_interval;
        let h = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await;
            let mut last_run_close_for: Option<chrono::NaiveDate> = None;
            let mut last_run_kline_for: Option<chrono::NaiveDate> = None;
            let mut last_run_daily_basic_for: Option<chrono::NaiveDate> = None;
            let mut last_run_events_for: Option<chrono::NaiveDate> = None;
            let mut last_run_universe_for: Option<chrono::NaiveDate> = None;
            loop {
                tokio::select! {
                    _ = stop_rx.recv() => break,
                    _ = ticker.tick() => {
                        let ctx = svc.market_time_now();
                        let shanghai_now = ctx.now.with_timezone(&Shanghai);
                        let cal: &dyn TradeCalendar = svc.calendar().as_ref();
                        let today = shanghai_now.date_naive();
                        let h = shanghai_now.time().hour();
                        let m = shanghai_now.time().minute();
                        let is_trade_day = cal.is_trade_day(today);

                        // 08:30 universe refresh（spec §5 line 795 "启动 + 每日 08:30"）
                        if h == 8 && m == 30
                            && last_run_universe_for != Some(today)
                            && is_trade_day
                        {
                            if let Err(e) = svc.refresh_market_instruments().await {
                                warn!(target: "quotes.scheduler.universe_daily", error = ?e, "08:30 universe refresh failed");
                            }
                            last_run_universe_for = Some(today);
                        }

                        // 09:00 daily_basic（最新已完成交易日）
                        if h == 9 && m == 0
                            && last_run_daily_basic_for != Some(today)
                            && is_trade_day
                        {
                            let req = RefreshDataScope::Universe;
                            if let Err(e) = svc.refresh_daily_basic(req, None).await {
                                warn!(target: "quotes.scheduler.daily_basic", error = ?e, "daily_basic refresh failed");
                            }
                            last_run_daily_basic_for = Some(today);
                        }

                        // 09:15 公司事件
                        if h == 9 && m == 15
                            && last_run_events_for != Some(today)
                            && is_trade_day
                        {
                            let req = RefreshDataScope::Universe;
                            if let Err(e) = svc.refresh_company_events(req, Some(30)).await {
                                warn!(target: "quotes.scheduler.events", error = ?e, "company events refresh failed");
                            }
                            last_run_events_for = Some(today);
                        }

                        // 15:30 close snapshot（盘后 30 分钟）
                        if h == 15 && m >= 30
                            && last_run_close_for != Some(today)
                            && is_trade_day
                        {
                            let req = RefreshMarketQuotesRequest {
                                scope: RefreshMarketQuotesScope::Universe,
                                purpose: RefreshPurpose::Close,
                                trade_date: None,
                            };
                            if let Err(e) = svc.refresh_market_quotes(req).await {
                                warn!(target: "quotes.scheduler.close", error = ?e, "close snapshot refresh failed");
                            }
                            last_run_close_for = Some(today);
                            // 启动重试 helper：5min 一次，最多 6 次。
                            let svc2 = Arc::clone(&svc);
                            let trade_date = ctx.latest_completed_trade_date;
                            tokio::spawn(async move {
                                for _ in 0..6 {
                                    tokio::time::sleep(Duration::from_secs(5 * 60)).await;
                                    if svc2.close_snapshot_complete(trade_date).await {
                                        return;
                                    }
                                    let req = RefreshMarketQuotesRequest {
                                        scope: RefreshMarketQuotesScope::Universe,
                                        purpose: RefreshPurpose::Close,
                                        trade_date: Some(trade_date),
                                    };
                                    if let Err(e) = svc2.refresh_market_quotes(req).await {
                                        warn!(target: "quotes.scheduler.close.retry", error = ?e, "retry failed");
                                    }
                                }
                            });
                        }

                        // 16:00 K 线预热（日 K + qfq/hfq）
                        if h == 16 && m == 0
                            && last_run_kline_for != Some(today)
                            && is_trade_day
                        {
                            let req = RefreshDataScope::Universe;
                            if let Err(e) = svc
                                .refresh_klines(
                                    req,
                                    vec![crate::domain::quotes::KlinePeriod::Day],
                                )
                                .await
                            {
                                warn!(target: "quotes.scheduler.kline", error = ?e, "kline refresh failed");
                            }
                            last_run_kline_for = Some(today);
                        }
                    }
                }
            }
        });
        joins.push(h);
        stops.push(stop_tx);
    }

    QuotesSchedulerHandle {
        _joins: joins,
        _stops: stops,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::db::run_migrations;
    use crate::infrastructure::quotes::{migrations as quotes_migrations, QuotesConfig};

    fn make_service() -> Arc<QuotesService> {
        let db = crate::infrastructure::db::AppDb::open_in_memory().unwrap();
        db.with(|conn| run_migrations(conn, quotes_migrations()).unwrap());
        Arc::new(QuotesService::new(db, QuotesConfig::default()).unwrap())
    }

    #[tokio::test]
    async fn intervals_default_has_subscribed_15s_and_universe_60s() {
        let i = QuotesSchedulerIntervals::default();
        assert_eq!(i.subscribed_interval.as_secs(), 15);
        assert_eq!(i.universe_interval.as_secs(), 60);
    }

    #[tokio::test]
    async fn scheduler_handles_drop_without_panic() {
        let svc = make_service();
        // 1s tick to short-circuit
        let handle = spawn_full_scheduler(
            svc,
            QuotesSchedulerIntervals {
                universe_interval: Duration::from_secs(3600),
                subscribed_interval: Duration::from_secs(3600),
                daily_tick_interval: Duration::from_secs(3600),
            },
        );
        drop(handle);
    }

    fn _shanghai_now() {
        let _ = Shanghai;
    }
}
