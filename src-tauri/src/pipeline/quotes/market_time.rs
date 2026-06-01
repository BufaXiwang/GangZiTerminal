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

    // ====================================================================== 盲区②
    // 时段矩阵（hermetic）：盘前 / 早盘 / 午休 / 午盘 / 盘后 / 周末 / 节假日。
    // 用 WeekdayCalendar（Mon-Fri 确定性日历）+ Holiday（全非交易日）注入，
    // 不依赖 wall-clock，全 hermetic。Spec: quotes-module.md §3 / §5。
    //
    // 锚定交易日：2026-05-26 = 周二（交易日），上一交易日 2026-05-25 = 周一。

    struct Holiday;
    impl TradeCalendar for Holiday {
        fn is_trade_day(&self, _: chrono::NaiveDate) -> bool {
            false
        }
    }

    #[test]
    fn premarket_0900_not_trading_latest_is_prev_day() {
        // 09:00 早于 09:30 开盘 → 非交易时段；当日是交易日故 current=今天；
        // 但 < 15:00（afternoon_close）→ latest_completed = 上一交易日。
        let ctx = resolve_market_time_with_calendar(sh(2026, 5, 26, 9, 0), &WeekdayCalendar);
        assert!(!ctx.is_trading_time, "09:00 盘前不应是交易时段");
        assert_eq!(
            ctx.current_trade_date.map(|d| d.format()),
            Some("20260526".into())
        );
        assert_eq!(ctx.latest_completed_trade_date.format(), "20260525");
    }

    #[test]
    fn morning_session_0935_is_trading() {
        let ctx = resolve_market_time_with_calendar(sh(2026, 5, 26, 9, 35), &WeekdayCalendar);
        assert!(ctx.is_trading_time, "09:35 早盘应是交易时段");
        assert_eq!(
            ctx.current_trade_date.map(|d| d.format()),
            Some("20260526".into())
        );
        // 盘中（< 15:00）latest_completed 仍是上一交易日。
        assert_eq!(ctx.latest_completed_trade_date.format(), "20260525");
    }

    #[test]
    fn lunch_break_1135_not_trading() {
        // 11:35 在 11:30 午休后、13:00 午盘前 → 非交易时段。
        let ctx = resolve_market_time_with_calendar(sh(2026, 5, 26, 11, 35), &WeekdayCalendar);
        assert!(!ctx.is_trading_time, "11:35 午休不应是交易时段");
        assert_eq!(ctx.latest_completed_trade_date.format(), "20260525");
    }

    #[test]
    fn morning_close_1130_boundary_is_not_trading() {
        // 边界：11:30:00 morning_close 用 `t < morning_close` 排除。
        let ctx = resolve_market_time_with_calendar(sh(2026, 5, 26, 11, 30), &WeekdayCalendar);
        assert!(!ctx.is_trading_time, "11:30 整应已收盘（半开区间）");
    }

    #[test]
    fn afternoon_session_1330_is_trading() {
        let ctx = resolve_market_time_with_calendar(sh(2026, 5, 26, 13, 30), &WeekdayCalendar);
        assert!(ctx.is_trading_time, "13:30 午盘应是交易时段");
        assert_eq!(ctx.latest_completed_trade_date.format(), "20260525");
    }

    #[test]
    fn afternoon_open_1300_boundary_is_trading() {
        // 边界：13:00:00 afternoon_open 用 `t >= afternoon_open` 含入。
        let ctx = resolve_market_time_with_calendar(sh(2026, 5, 26, 13, 0), &WeekdayCalendar);
        assert!(ctx.is_trading_time, "13:00 整应已开盘");
    }

    #[test]
    fn post_market_1530_not_trading_latest_flips_to_today() {
        // 15:30 在 15:00 收盘后 → 非交易；且 t >= afternoon_close → latest_completed = 今天。
        let ctx = resolve_market_time_with_calendar(sh(2026, 5, 26, 15, 30), &WeekdayCalendar);
        assert!(!ctx.is_trading_time, "15:30 盘后不应是交易时段");
        assert_eq!(
            ctx.latest_completed_trade_date.format(),
            "20260526",
            "收盘后 latest_completed 应翻为今日"
        );
    }

    #[test]
    fn afternoon_close_1500_boundary_is_not_trading_and_latest_is_today() {
        // 边界：15:00:00 afternoon_close 用 `t < afternoon_close` 排除 →
        // 非交易，且因 `t < afternoon_close` 为 false，latest_completed = 今天。
        let ctx = resolve_market_time_with_calendar(sh(2026, 5, 26, 15, 0), &WeekdayCalendar);
        assert!(!ctx.is_trading_time, "15:00 整应已收盘");
        assert_eq!(ctx.latest_completed_trade_date.format(), "20260526");
    }

    #[test]
    fn saturday_is_not_trading_and_latest_is_friday() {
        // 2026-05-30 周六：WeekdayCalendar 判非交易日 → is_trading_time=false，
        // current_trade_date=None，latest_completed 向前找到 2026-05-29 周五。
        let ctx = resolve_market_time_with_calendar(sh(2026, 5, 30, 10, 30), &WeekdayCalendar);
        assert!(!ctx.is_trading_time, "周六非交易日");
        assert!(ctx.current_trade_date.is_none());
        assert_eq!(ctx.latest_completed_trade_date.format(), "20260529");
        // next_trade_date：周六向后找到下周一 2026-06-01。
        assert_eq!(
            ctx.next_trade_date.map(|d| d.format()),
            Some("20260601".into())
        );
    }

    #[test]
    fn holiday_calendar_overrides_weekday_trading_time() {
        // 节假日（注入全非交易日历）：即便在 10:00 早盘时间窗内也不交易。
        let ctx = resolve_market_time_with_calendar(sh(2026, 5, 26, 10, 0), &Holiday);
        assert!(!ctx.is_trading_time, "节假日不应是交易时段");
        assert!(ctx.current_trade_date.is_none());
        // 全非交易日历：latest_completed 365 次向前找不到 → 回退到初始猜测（今日）。
        // next_trade_date 30 次向后找不到 → None。
        assert!(ctx.next_trade_date.is_none(), "全节假日历 next_trade_date 应为 None");
    }

    #[test]
    fn next_trade_date_friday_points_to_next_monday() {
        // 周五 2026-05-29 盘后：下一交易日跨过周末到周一 2026-06-01。
        let ctx = resolve_market_time_with_calendar(sh(2026, 5, 29, 16, 0), &WeekdayCalendar);
        assert_eq!(
            ctx.next_trade_date.map(|d| d.format()),
            Some("20260601".into())
        );
    }
}
