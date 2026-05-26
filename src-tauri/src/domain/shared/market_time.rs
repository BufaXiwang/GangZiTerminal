//! `MarketTimeContext` + `resolve_market_time(now)` —— spec `shared-types.md §3`.
//!
//! A 股交易时段语义：
//! - 上午连续竞价 09:30–11:30 Asia/Shanghai
//! - 下午连续竞价 13:00–15:00 Asia/Shanghai
//! - 集合竞价、午休、收盘后、周末、节假日均视为 `isTradingTime = false`
//!
//! Quotes / Account / Agent 凡是涉及行情有效性 / 即时成交判断的地方都必须
//! 使用此函数，不能各自手写时段判断。
//!
//! 当前阶段简化：节假日识别走"周末跳过 + 自然交易日推导"——后续接入完整
//! 交易日历后改为权威 calendar 查询，对外接口稳定不变。

use chrono::{Datelike, Duration, NaiveDate, Timelike, Weekday};
use serde::{Deserialize, Serialize};

use super::time::{OccurredAt, TradeDate};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketTimeContext {
    pub now: OccurredAt,
    pub is_trading_time: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_trade_date: Option<TradeDate>,
    pub latest_completed_trade_date: TradeDate,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_trade_date: Option<TradeDate>,
}

/// 解析当前市场时段。`now` 为本地 UTC 时刻（ms）。
///
/// `current_trade_date`：当且仅当当前自然日是交易日时返回——盘前 / 午休 /
/// 盘后仍可返回当日交易日，集合竞价归入当日交易日的 currentTradeDate。
pub fn resolve_market_time(now: OccurredAt) -> MarketTimeContext {
    let dt_utc = chrono::DateTime::from_timestamp_millis(now.value())
        .unwrap_or_else(|| chrono::Utc::now());
    let beijing = dt_utc + Duration::hours(8);
    let beijing_naive = beijing.naive_utc();
    let weekday = beijing_naive.weekday();
    let date = beijing_naive.date();
    let is_weekday = !matches!(weekday, Weekday::Sat | Weekday::Sun);
    let minute_of_day = beijing_naive.hour() * 60 + beijing_naive.minute();
    let is_trading_time = is_weekday
        && ((570..=690).contains(&minute_of_day) || (780..=900).contains(&minute_of_day));

    let current_trade_date = if is_weekday {
        Some(TradeDate::from_unchecked(naive_to_yyyymmdd(date)))
    } else {
        None
    };
    let latest_completed_trade_date = TradeDate::from_unchecked(naive_to_yyyymmdd(
        latest_completed(date, beijing_naive.hour() * 60 + beijing_naive.minute()),
    ));
    let next_trade_date = Some(TradeDate::from_unchecked(naive_to_yyyymmdd(next_weekday(
        date,
    ))));

    MarketTimeContext {
        now,
        is_trading_time,
        current_trade_date,
        latest_completed_trade_date,
        next_trade_date,
    }
}

fn naive_to_yyyymmdd(d: NaiveDate) -> i32 {
    d.year() * 10000 + d.month() as i32 * 100 + d.day() as i32
}

/// 最近一个已完成交易日。
/// - 周一至周五 15:00 之后 / 周末：当周最近一个 ≤ 当日的工作日
/// - 周一至周五 15:00 之前：上一个工作日
fn latest_completed(today: NaiveDate, minute_of_day: u32) -> NaiveDate {
    let closed_today = minute_of_day >= 900 && !matches!(today.weekday(), Weekday::Sat | Weekday::Sun);
    let mut d = if closed_today {
        today
    } else {
        today - Duration::days(1)
    };
    while matches!(d.weekday(), Weekday::Sat | Weekday::Sun) {
        d -= Duration::days(1);
    }
    d
}

fn next_weekday(today: NaiveDate) -> NaiveDate {
    let mut d = today + Duration::days(1);
    while matches!(d.weekday(), Weekday::Sat | Weekday::Sun) {
        d += Duration::days(1);
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_beijing(yyyy: i32, mm: u32, dd: u32, hh: u32, mi: u32) -> OccurredAt {
        let dt = chrono::NaiveDate::from_ymd_opt(yyyy, mm, dd)
            .unwrap()
            .and_hms_opt(hh, mi, 0)
            .unwrap()
            - Duration::hours(8);
        OccurredAt::new(dt.and_utc().timestamp_millis())
    }

    #[test]
    fn weekday_morning_session_is_trading() {
        // 2026-01-05 周一 10:00 北京
        let ctx = resolve_market_time(at_beijing(2026, 1, 5, 10, 0));
        assert!(ctx.is_trading_time);
        assert!(ctx.current_trade_date.is_some());
    }

    #[test]
    fn weekday_lunch_is_not_trading() {
        let ctx = resolve_market_time(at_beijing(2026, 1, 5, 12, 0));
        assert!(!ctx.is_trading_time);
        assert!(ctx.current_trade_date.is_some());
    }

    #[test]
    fn saturday_no_current_trade_date() {
        // 2026-01-10 周六
        let ctx = resolve_market_time(at_beijing(2026, 1, 10, 10, 0));
        assert!(!ctx.is_trading_time);
        assert!(ctx.current_trade_date.is_none());
    }

    #[test]
    fn after_close_uses_today_as_completed() {
        // 周一 16:00：今日已收盘
        let ctx = resolve_market_time(at_beijing(2026, 1, 5, 16, 0));
        assert!(!ctx.is_trading_time);
        // latest_completed_trade_date should be today 20260105
        assert_eq!(ctx.latest_completed_trade_date.to_compact(), "20260105");
    }

    #[test]
    fn before_open_uses_previous_workday() {
        // 周一 08:00：上一个工作日是上周五
        let ctx = resolve_market_time(at_beijing(2026, 1, 5, 8, 0));
        assert!(!ctx.is_trading_time);
        // 2026-01-02 周五
        assert_eq!(ctx.latest_completed_trade_date.to_compact(), "20260102");
    }
}
