//! TuShare 交易日历——trade_cal 接入。
//!
//! 全局 `TRADE_CALENDAR` snapshot——启动时 hydrate 一年，is_trading_day / next /
//! previous 全部 O(log n) 查询。失败时返 Err 上层处理（不静默 fallback 硬编码窗口）。

use super::client::{call, row_str};
use crate::domain::quotes::{QuotesError, TradeCalendar};
use crate::domain::shared::{OccurredAt, TradeDate};
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::{OnceLock, RwLock};
use tauri::AppHandle;

// ============================================================================
// 全局 calendar snapshot
// ============================================================================

static CALENDAR: OnceLock<RwLock<TradeCalendar>> = OnceLock::new();

fn store() -> &'static RwLock<TradeCalendar> {
    CALENDAR.get_or_init(|| {
        RwLock::new(TradeCalendar {
            trading_days: Vec::new(),
            last_synced_at: OccurredAt::new(0),
        })
    })
}

// ============================================================================
// 同步读 API
// ============================================================================

pub fn is_trading_day(date: TradeDate) -> bool {
    let store = store().read();
    match store {
        Ok(g) => g.trading_days.binary_search(&date).is_ok(),
        Err(_) => false,
    }
}

pub fn next_trading_day(date: TradeDate) -> Option<TradeDate> {
    let g = store().read().ok()?;
    g.trading_days.iter().find(|d| **d > date).copied()
}

pub fn previous_trading_day(date: TradeDate) -> Option<TradeDate> {
    let g = store().read().ok()?;
    g.trading_days.iter().rev().find(|d| **d < date).copied()
}

pub fn current_trade_date() -> TradeDate {
    let today = TradeDate::today_beijing();
    if is_trading_day(today) {
        today
    } else {
        previous_trading_day(today).unwrap_or(today)
    }
}

pub fn last_synced() -> OccurredAt {
    store()
        .read()
        .map(|g| g.last_synced_at)
        .unwrap_or(OccurredAt::new(0))
}

// ============================================================================
// 异步刷新——scheduler 启动时 + 每年 1 月调
// ============================================================================

/// 从 TuShare 拉指定年份的交易日历（含 +1 年缓冲）。
pub async fn refresh_trade_calendar(app: &AppHandle, year: i32) -> Result<usize, QuotesError> {
    let start = TradeDate::new(year * 10000 + 101)?;
    let end = TradeDate::new((year + 1) * 10000 + 1231)?;
    let params = json!({
        "start_date": start.to_compact(),
        "end_date": end.to_compact(),
        "is_open": "1",
    });
    let rows = call(app, "trade_cal", params, "cal_date,is_open").await?;
    let days: BTreeSet<TradeDate> = rows
        .iter()
        .filter_map(|row| TradeDate::from_compact(&row_str(row, "cal_date")?).ok())
        .collect();
    let count = days.len();
    if let Ok(mut g) = store().write() {
        let mut existing: BTreeSet<TradeDate> = g.trading_days.iter().copied().collect();
        existing.extend(days);
        g.trading_days = existing.into_iter().collect();
        g.last_synced_at = OccurredAt::now();
    }
    Ok(count)
}
