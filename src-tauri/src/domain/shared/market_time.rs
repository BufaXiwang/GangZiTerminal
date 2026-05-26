//! 交易日历 / 市场时间上下文。
//!
//! Spec: docs/design/shared-types.md §3
//!
//! A 股默认连续竞价时段：09:30-11:30 / 13:00-15:00 Asia/Shanghai。
//!
//! 规则：
//! - 交易时间判断必须基于交易日历和 Asia/Shanghai。
//! - 集合竞价、午休、收盘后、周末和节假日都视为 `isTradingTime = false`。
//! - `currentTradeDate` 只在当前自然日是交易日时返回。
//! - `latestCompletedTradeDate` 表示最近一个已经完成收盘的交易日。
//!
//! ⚠️ 注意：本实现尚未接入完整 A 股节假日日历——只按"周一到周五 = 交易日"近似处理。
//! Quotes 模块需要在 Phase 1 接入正式交易日历后替换 [`is_trade_day`] 实现。

use super::types::{OccurredAt, TradeDate};
use chrono::{Datelike, NaiveTime, Weekday};
use chrono_tz::Asia::Shanghai;
use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
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

/// Spec: shared-types.md §3
pub fn resolve_market_time(now: OccurredAt) -> MarketTimeContext {
    let shanghai = now.with_timezone(&Shanghai);
    let today_naive = shanghai.date_naive();
    let t = shanghai.time();

    let session_morning_open = NaiveTime::from_hms_opt(9, 30, 0).unwrap();
    let session_morning_close = NaiveTime::from_hms_opt(11, 30, 0).unwrap();
    let session_afternoon_open = NaiveTime::from_hms_opt(13, 0, 0).unwrap();
    let session_afternoon_close = NaiveTime::from_hms_opt(15, 0, 0).unwrap();

    let today_is_trade = is_trade_day(today_naive);

    let is_trading_time = today_is_trade
        && ((t >= session_morning_open && t < session_morning_close)
            || (t >= session_afternoon_open && t < session_afternoon_close));

    let current_trade_date = if today_is_trade {
        Some(TradeDate::from_naive(today_naive))
    } else {
        None
    };

    let latest_completed_trade_date = {
        let mut d = today_naive;
        if today_is_trade && t < session_afternoon_close {
            d = d.pred_opt().expect("date underflow");
        }
        loop {
            if is_trade_day(d) {
                break TradeDate::from_naive(d);
            }
            d = d.pred_opt().expect("date underflow");
        }
    };

    let next_trade_date = {
        let mut d = today_naive.succ_opt().expect("date overflow");
        let mut found = None;
        for _ in 0..14 {
            if is_trade_day(d) {
                found = Some(TradeDate::from_naive(d));
                break;
            }
            d = d.succ_opt().expect("date overflow");
        }
        found
    };

    MarketTimeContext {
        now,
        is_trading_time,
        current_trade_date,
        latest_completed_trade_date,
        next_trade_date,
    }
}

/// 近似实现：周一到周五 = 交易日。
///
/// ⚠️ 不含 A 股节假日日历。Quotes 模块 Phase 1 必须替换为正式日历。
pub(crate) fn is_trade_day(d: chrono::NaiveDate) -> bool {
    !matches!(d.weekday(), Weekday::Sat | Weekday::Sun)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sh(y: i32, m: u32, d: u32, hh: u32, mm: u32) -> OccurredAt {
        Shanghai
            .with_ymd_and_hms(y, m, d, hh, mm, 0)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn weekday_morning_session_is_trading() {
        // 2026-05-26 周二 10:00 Asia/Shanghai
        let ctx = resolve_market_time(sh(2026, 5, 26, 10, 0));
        assert!(ctx.is_trading_time);
        assert_eq!(
            ctx.current_trade_date.unwrap().format(),
            "20260526"
        );
    }

    #[test]
    fn weekday_lunch_is_not_trading() {
        let ctx = resolve_market_time(sh(2026, 5, 26, 12, 0));
        assert!(!ctx.is_trading_time);
    }

    #[test]
    fn weekend_is_not_trading() {
        // 2026-05-23 周六
        let ctx = resolve_market_time(sh(2026, 5, 23, 10, 0));
        assert!(!ctx.is_trading_time);
        assert!(ctx.current_trade_date.is_none());
        // latest completed = 2026-05-22 周五
        assert_eq!(ctx.latest_completed_trade_date.format(), "20260522");
    }

    #[test]
    fn pre_market_latest_is_yesterday() {
        // 2026-05-26 周二 08:00 — 盘前
        let ctx = resolve_market_time(sh(2026, 5, 26, 8, 0));
        assert!(!ctx.is_trading_time);
        // 当日仍是交易日
        assert!(ctx.current_trade_date.is_some());
        // 最近完成收盘是 2026-05-25 周一
        assert_eq!(ctx.latest_completed_trade_date.format(), "20260525");
    }

    #[test]
    fn after_close_latest_is_today() {
        // 2026-05-26 周二 15:30 — 收盘后
        let ctx = resolve_market_time(sh(2026, 5, 26, 15, 30));
        assert!(!ctx.is_trading_time);
        assert_eq!(ctx.latest_completed_trade_date.format(), "20260526");
    }
}
