//! Quotes 后台调度。
//!
//! Spec: docs/design/quotes-module.md §5 后台刷新。
//!
//! 调度档：
//! - universe 滚动刷新：唯一后台报价任务。把全市场切 80 只/批，按固定 cycle
//!   （默认 30s）滚动轮刷——每 roll tick（1s）推 `ceil(批数 / cycle_secs)` 个
//!   80-批，cursor 前进、到末尾 wrap，只在 `is_in_quote_refresh_window` 内跑。
//! - 收盘快照 15:30 触发（close_snapshot）
//!   + 5min 重试，最多 6 次，直到当日完整完成。
//! - 16:00 K 线预热（universe daily K + qfq/hfq）
//! - 每日 09:00 daily_basic（上一交易日）
//! - 每日 09:15 公司事件（dividends + suspensions T-3..T+30）

use crate::domain::quotes::{RefreshDataScope, RefreshMarketQuotesScope, RefreshPurpose};
use crate::infrastructure::quotes::TradeCalendar;
use crate::pipeline::quotes::service::{QuotesService, RefreshMarketQuotesRequest};
use chrono::Timelike;
use chrono_tz::Asia::Shanghai;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

/// universe 滚动 batch 大小（spec §5 / §连接池：80 只/批）。
pub const QUOTES_ROLL_BATCH: usize = 80;
/// universe 滚动周期（spec §5：默认 30s，全市场每 ~30s 滚一轮，可配 10–60s）。
pub const QUOTES_UNIVERSE_ROLLING_CYCLE_SECS: u64 = 30;
/// 滚动 tick 间隔（固定小间隔 1s；每 tick 推 ceil(批数 / cycle_secs) 个 batch）。
pub const QUOTES_ROLL_TICK_SECS: u64 = 1;

pub struct QuotesSchedulerHandle {
    _joins: Vec<JoinHandle<()>>,
    _stops: Vec<mpsc::Sender<()>>,
}

/// 单 tick scheduler — 兼容旧 API，等价于 universe 滚动（roll tick = 传入 interval）。
pub fn spawn_quotes_scheduler(
    service: Arc<QuotesService>,
    interval: Duration,
) -> QuotesSchedulerHandle {
    spawn_full_scheduler(
        service,
        QuotesSchedulerIntervals {
            roll_tick_interval: interval,
            universe_rolling_cycle: Duration::from_secs(QUOTES_UNIVERSE_ROLLING_CYCLE_SECS),
            daily_tick_interval: Duration::from_secs(60),
        },
    )
}

#[derive(Debug, Clone)]
pub struct QuotesSchedulerIntervals {
    /// 滚动 tick 固定小间隔（默认 1s）。
    pub roll_tick_interval: Duration,
    /// universe 全量滚一轮的目标周期（默认 30s，可配 10–60s）。
    pub universe_rolling_cycle: Duration,
    /// 日常时间窗口 tick 间隔（默认 60s：检查 15:30/16:00/08:30/09:00/09:15）。
    pub daily_tick_interval: Duration,
}

impl Default for QuotesSchedulerIntervals {
    fn default() -> Self {
        Self {
            roll_tick_interval: Duration::from_secs(QUOTES_ROLL_TICK_SECS),
            universe_rolling_cycle: Duration::from_secs(QUOTES_UNIVERSE_ROLLING_CYCLE_SECS),
            daily_tick_interval: Duration::from_secs(60),
        }
    }
}

/// 一次滚动 tick 推多少个 80-batch（纯函数，便于单测）。
///
/// 让全市场 `num_batches` 个 batch 在 `cycle_secs` 秒内（每 `tick_secs` 一 tick）滚完一轮：
/// `batches_per_tick = ceil(num_batches / (cycle_secs / tick_secs))`，至少 1（universe 非空时）。
///
/// Spec: docs/design/quotes-module.md §5「每 ~cycle/批数 推一批」。
pub fn batches_per_tick(num_batches: usize, cycle_secs: u64, tick_secs: u64) -> usize {
    if num_batches == 0 {
        return 0;
    }
    let tick_secs = tick_secs.max(1);
    let ticks_per_cycle = (cycle_secs / tick_secs).max(1);
    // ceil(num_batches / ticks_per_cycle)
    let n = (num_batches as u64).div_ceil(ticks_per_cycle);
    (n as usize).max(1)
}

/// 计算从 `num_batches` 中、从 `cursor` 起取 `count` 个 batch 的 ts_code 切片（含 wrap）。
/// 返回 (各 batch 的 (start, end) 半开区间, 推进后的 cursor)。纯函数，便于单测 cursor 推进。
fn next_batch_ranges(
    universe_len: usize,
    cursor: usize,
    count: usize,
) -> (Vec<(usize, usize)>, usize) {
    if universe_len == 0 || count == 0 {
        return (Vec::new(), cursor);
    }
    let num_batches = universe_len.div_ceil(QUOTES_ROLL_BATCH);
    let mut cur = cursor % num_batches;
    let mut ranges = Vec::with_capacity(count);
    for _ in 0..count {
        let start = cur * QUOTES_ROLL_BATCH;
        let end = (start + QUOTES_ROLL_BATCH).min(universe_len);
        ranges.push((start, end));
        cur = (cur + 1) % num_batches;
    }
    (ranges, cur)
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

    // ----- universe 滚动刷新 tick（spec §5：唯一后台报价任务）
    //
    // 维护 cursor over `universe_quote_targets()`（Stock→Index→Fund 有序）。每 roll tick
    // 在 `is_in_quote_refresh_window` 内推 `batches_per_tick` 个 80-批，cursor 前进、wrap。
    // 每批走 `refresh_quote_batch`（新鲜度跳过 + 80批并发 + per-batch emit progress），
    // 不每批 emit 完整 refreshed。universe 大小变化时按当前列表长度重算批数。
    {
        let svc = Arc::clone(&service);
        let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let tick_interval = intervals.roll_tick_interval;
        let cycle_secs = intervals.universe_rolling_cycle.as_secs().max(1);
        let tick_secs = tick_interval.as_secs().max(1);
        let h = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(tick_interval);
            ticker.tick().await;
            let mut cursor: usize = 0;
            loop {
                tokio::select! {
                    _ = stop_rx.recv() => break,
                    _ = ticker.tick() => {
                        let ctx = svc.market_time_now();
                        if !ctx.is_in_quote_refresh_window {
                            // 非刷新窗：cursor 不动、不刷。
                            continue;
                        }
                        let targets = svc.universe_quote_targets();
                        if targets.is_empty() {
                            continue;
                        }
                        let num_batches = targets.len().div_ceil(QUOTES_ROLL_BATCH);
                        let count = batches_per_tick(num_batches, cycle_secs, tick_secs);
                        let (ranges, next_cursor) =
                            next_batch_ranges(targets.len(), cursor, count);
                        cursor = next_cursor;
                        for (start, end) in ranges {
                            let slice: Vec<_> = targets[start..end]
                                .iter()
                                .map(|(ts, _, _)| ts.clone())
                                .collect();
                            // refresh_quote_batch：progress-only（不 emit 完整 refreshed）。
                            let _ = svc.refresh_quote_batch(slice).await;
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
                        // Spec: quotes-module.md §2 "TuShare 健康状态"：每 60s 检查是否到 recheck 时刻。
                        // service 内部按 `next_recheck_at` (默认 1h 间隔) 判定 due/not-due，本 tick 是廉价的。
                        svc.health().recheck_if_due().await;

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
    async fn intervals_default_is_rolling_1s_tick_30s_cycle() {
        // Spec §5：滚动 tick 1s、universe cycle 30s、daily tick 60s。
        let i = QuotesSchedulerIntervals::default();
        assert_eq!(i.roll_tick_interval.as_secs(), 1);
        assert_eq!(i.universe_rolling_cycle.as_secs(), 30);
        assert_eq!(i.daily_tick_interval.as_secs(), 60);
        assert_eq!(QUOTES_UNIVERSE_ROLLING_CYCLE_SECS, 30);
        assert_eq!(QUOTES_ROLL_BATCH, 80);
        assert_eq!(QUOTES_ROLL_TICK_SECS, 1);
    }

    #[test]
    fn batches_per_tick_spreads_full_universe_over_cycle() {
        // 全市场 ~7500 只 → ~94 个 80-批；30s cycle / 1s tick = 30 ticks/cycle。
        // 每 tick ceil(94/30) = 4 个 batch → 一轮 ~24s ≤ 30s。
        assert_eq!(batches_per_tick(94, 30, 1), 4);
        // 小 universe：批数 < ticks_per_cycle → 每 tick 至少推 1 个。
        assert_eq!(batches_per_tick(10, 30, 1), 1);
        // 空 universe → 0。
        assert_eq!(batches_per_tick(0, 30, 1), 0);
        // 整除场景：60 批 / 30 ticks = 每 tick 2。
        assert_eq!(batches_per_tick(60, 30, 1), 2);
        // tick_secs 防 0（max(1)）。
        assert_eq!(batches_per_tick(30, 30, 0), 1);
    }

    #[test]
    fn next_batch_ranges_advances_and_wraps() {
        // universe = 200 只 → 3 个 80-批（[0,80),[80,160),[160,200)）。
        let (r0, c0) = next_batch_ranges(200, 0, 2);
        assert_eq!(r0, vec![(0, 80), (80, 160)]);
        assert_eq!(c0, 2);
        // 从 cursor=2 取 2 个：批 2（[160,200)）→ wrap 回批 0（[0,80)）。
        let (r1, c1) = next_batch_ranges(200, c0, 2);
        assert_eq!(r1, vec![(160, 200), (0, 80)]);
        assert_eq!(c1, 1);
        // 空 universe / count=0 → 空、cursor 不动。
        assert_eq!(next_batch_ranges(0, 5, 3), (Vec::new(), 5));
        assert_eq!(next_batch_ranges(200, 1, 0), (Vec::new(), 1));
    }

    #[tokio::test]
    async fn scheduler_handles_drop_without_panic() {
        let svc = make_service();
        // 大 interval to short-circuit ticks before drop。
        let handle = spawn_full_scheduler(
            svc,
            QuotesSchedulerIntervals {
                roll_tick_interval: Duration::from_secs(3600),
                universe_rolling_cycle: Duration::from_secs(30),
                daily_tick_interval: Duration::from_secs(3600),
            },
        );
        drop(handle);
    }

    fn _shanghai_now() {
        let _ = Shanghai;
    }
}
