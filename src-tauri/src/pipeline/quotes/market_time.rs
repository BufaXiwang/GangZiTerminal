//! 基于本地 TuShare 交易日历修正 shared `resolve_market_time` 的产物。
//!
//! Spec: docs/design/quotes-module.md §3 / §5；docs/design/shared-types.md §3
//!
//! shared `resolve_market_time` 用"周一到周五"近似日历（spec 已在 §🚨 反馈）。Quotes BC 内部
//! 必须用正式日历——本 helper 在 Quotes 侧重新派生 `is_trading_time` /
//! `current_trade_date` / `latest_completed_trade_date` / `next_trade_date`。
//!
//! Quotes 对外 API 一律走这个 helper；shared 的近似版只是初始化默认。

use crate::domain::shared::{resolve_market_time, MarketTimeContext, OccurredAt, TradeDate};
use crate::infrastructure::quotes::TradeCalendar;
use chrono::{NaiveTime, Timelike};
use chrono_tz::Asia::Shanghai;

/// 用 `TradeCalendar` 修正 shared `resolve_market_time(now)` 的输出。
///
/// 算法：
/// - 用日历推 today 是否交易日；非交易日 `is_trading_time = false`、`current_trade_date = None`。
/// - `latest_completed_trade_date`：若今日是交易日且当前 < 15:00，则 = 上一个交易日；否则 today。
///   今日不是交易日，向前找最近交易日。
/// - `next_trade_date`：向后找最近一个交易日。
pub fn resolve_market_time_with_calendar(
    now: OccurredAt,
    calendar: &dyn TradeCalendar,
) -> MarketTimeContext {
    let base = resolve_market_time(now);
    let shanghai = now.with_timezone(&Shanghai);
    let today_naive = shanghai.date_naive();
    let t = shanghai.time();

    let morning_open = NaiveTime::from_hms_opt(9, 30, 0).unwrap();
    let morning_close = NaiveTime::from_hms_opt(11, 30, 0).unwrap();
    let afternoon_open = NaiveTime::from_hms_opt(13, 0, 0).unwrap();
    let afternoon_close = NaiveTime::from_hms_opt(15, 0, 0).unwrap();

    let today_is_trade = calendar.is_trade_day(today_naive);
    let is_trading_time = today_is_trade
        && ((t >= morning_open && t < morning_close)
            || (t >= afternoon_open && t < afternoon_close));

    let current_trade_date = if today_is_trade {
        Some(TradeDate::from_naive(today_naive))
    } else {
        None
    };

    let latest_completed_trade_date = {
        let mut d = today_naive;
        if today_is_trade && t < afternoon_close {
            d = d.pred_opt().expect("date underflow");
        }
        let mut found = TradeDate::from_naive(d);
        for _ in 0..365 {
            if calendar.is_trade_day(d) {
                found = TradeDate::from_naive(d);
                break;
            }
            d = match d.pred_opt() {
                Some(v) => v,
                None => break,
            };
        }
        found
    };

    let next_trade_date = {
        let mut d = today_naive.succ_opt().expect("date overflow");
        let mut out = None;
        for _ in 0..30 {
            if calendar.is_trade_day(d) {
                out = Some(TradeDate::from_naive(d));
                break;
            }
            d = d.succ_opt().expect("date overflow");
        }
        out
    };

    let _ = base;
    let _ = afternoon_close.hour();
    MarketTimeContext {
        now,
        is_trading_time,
        current_trade_date,
        latest_completed_trade_date,
        next_trade_date,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::quotes::WeekdayCalendar;
    use chrono::TimeZone;

    fn sh(y: i32, m: u32, d: u32, hh: u32, mm: u32) -> OccurredAt {
        Shanghai
            .with_ymd_and_hms(y, m, d, hh, mm, 0)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn calendar_says_holiday_overrides_trading_time() {
        struct Holiday;
        impl TradeCalendar for Holiday {
            fn is_trade_day(&self, _: chrono::NaiveDate) -> bool {
                false
            }
        }
        let ctx = resolve_market_time_with_calendar(sh(2026, 5, 26, 10, 0), &Holiday);
        assert!(!ctx.is_trading_time);
        assert!(ctx.current_trade_date.is_none());
    }

    #[test]
    fn weekday_calendar_matches_shared_resolve() {
        let ctx = resolve_market_time_with_calendar(sh(2026, 5, 26, 10, 0), &WeekdayCalendar);
        assert!(ctx.is_trading_time);
    }

    #[test]
    fn pre_market_latest_is_previous_trade_day() {
        let ctx = resolve_market_time_with_calendar(sh(2026, 5, 26, 8, 0), &WeekdayCalendar);
        assert_eq!(ctx.latest_completed_trade_date.format(), "20260525");
    }
}
