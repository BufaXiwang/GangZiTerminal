//! Freshness 规则 — eligible trade date / stale threshold / hard expire。
//!
//! Spec: docs/design/quotes-module.md §2 (行情快照 / freshness 定义)

use crate::domain::shared::{
    Freshness, FreshnessStatus, MarketTimeContext, OccurredAt, TradeDate, WarningCode,
};

/// stale threshold（精确读取）。spec §2: detail 默认 30s。
pub const DETAIL_STALE_THRESHOLD_SECS: i64 = 30;
/// stale threshold（全市场 / 大范围读取）。spec §2: universe 默认 90s。
pub const UNIVERSE_STALE_THRESHOLD_SECS: i64 = 90;
/// 交易时段硬过期：spec §2 默认 1 小时。
pub const HARD_EXPIRE_SECS: i64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshnessIntent {
    /// `fetch_data(include.quote = true)` 等精确读取。
    Detail,
    /// `list_market(includeQuote = true)` / `scan_market` 等全市场读取。
    Universe,
}

impl FreshnessIntent {
    pub fn stale_threshold_secs(self) -> i64 {
        match self {
            FreshnessIntent::Detail => DETAIL_STALE_THRESHOLD_SECS,
            FreshnessIntent::Universe => UNIVERSE_STALE_THRESHOLD_SECS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EligibleTradeDate {
    pub trade_date: TradeDate,
    /// true 表示当前在交易时段；false 表示非交易时段（取最新已完成交易日）。
    pub is_intraday: bool,
}

/// Spec: quotes-module.md §2 — 每个读取请求只调用一次 `resolve_market_time(now)`，并由
/// 该结果计算唯一 eligible trade date。
pub fn eligible_trade_date(ctx: &MarketTimeContext) -> EligibleTradeDate {
    if ctx.is_trading_time {
        // 交易时段：必须有 currentTradeDate（spec §3 guarantee）。
        let td = ctx
            .current_trade_date
            .unwrap_or(ctx.latest_completed_trade_date);
        EligibleTradeDate {
            trade_date: td,
            is_intraday: true,
        }
    } else {
        EligibleTradeDate {
            trade_date: ctx.latest_completed_trade_date,
            is_intraday: false,
        }
    }
}

/// Spec: quotes-module.md §2 — 派生对外 Freshness。
///
/// 输入：
/// - `ctx` 当前请求的 MarketTimeContext（每次请求只调用一次 resolve_market_time）。
/// - `intent` 读取意图（detail vs universe），决定 stale threshold。
/// - `quote_trade_date` quote 自身的 tradeDate。
/// - `captured_at` quote 写入 snapshot 的时间。
/// - `source` quote 来源（tdx/eastmoney/...）。
///
/// 返回 `(Freshness, Option<eligibility-warning>)`：
/// - `Freshness.status = "missing"` 表示当前 trade date 不匹配或硬过期。
/// - `Freshness.warning = SnapshotExpired / QuoteMissing` 在 missing 场景下填充。
/// - 第二个返回值为 None 表示 snapshot 可作为有效行情字段使用；Some 表示 quote 字段必须置空。
pub fn derive_freshness(
    ctx: &MarketTimeContext,
    intent: FreshnessIntent,
    quote_trade_date: TradeDate,
    captured_at: OccurredAt,
    source: &str,
) -> (Freshness, Option<WarningCode>) {
    let eligible = eligible_trade_date(ctx);
    let age_ms = (ctx.now - captured_at).num_milliseconds();
    let age_secs = age_ms / 1000;

    // tradeDate 不匹配 eligible trade date → 不可用。
    if quote_trade_date != eligible.trade_date {
        let warning = if eligible.is_intraday {
            WarningCode::QuoteMissing
        } else {
            WarningCode::SnapshotExpired
        };
        return (
            Freshness {
                status: FreshnessStatus::Missing,
                captured_at: Some(captured_at),
                exchange_time: None,
                age_ms: Some(age_ms),
                source: Some(source.to_string()),
                warning: Some(warning),
            },
            Some(warning),
        );
    }

    // 交易时段硬过期。
    if eligible.is_intraday && age_secs > HARD_EXPIRE_SECS {
        return (
            Freshness {
                status: FreshnessStatus::Missing,
                captured_at: Some(captured_at),
                exchange_time: None,
                age_ms: Some(age_ms),
                source: Some(source.to_string()),
                warning: Some(WarningCode::SnapshotExpired),
            },
            Some(WarningCode::SnapshotExpired),
        );
    }

    // stale 判断只在交易时段；非交易时段只要 tradeDate 匹配最新已完成交易日，即视为 fresh
    // close fact，不因 age 自动 stale（spec §2 + shared-types §3）。
    let status = if eligible.is_intraday && age_secs > intent.stale_threshold_secs() {
        FreshnessStatus::Stale
    } else {
        FreshnessStatus::Fresh
    };
    let warning = if matches!(status, FreshnessStatus::Stale) {
        Some(WarningCode::QuoteStale)
    } else {
        None
    };

    (
        Freshness {
            status,
            captured_at: Some(captured_at),
            exchange_time: None,
            age_ms: Some(age_ms),
            source: Some(source.to_string()),
            warning,
        },
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use chrono_tz::Asia::Shanghai;

    fn sh(y: i32, m: u32, d: u32, hh: u32, mm: u32) -> OccurredAt {
        Shanghai
            .with_ymd_and_hms(y, m, d, hh, mm, 0)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn sh_secs(y: i32, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> OccurredAt {
        Shanghai
            .with_ymd_and_hms(y, m, d, hh, mm, ss)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn intraday_fresh_within_threshold() {
        let now = sh(2026, 5, 26, 10, 0);
        let ctx = crate::domain::shared::resolve_market_time(now);
        let td = TradeDate::parse("20260526").unwrap();
        // DETAIL 阈值 30s；captured 比 now 早 10s，应判 Fresh。
        let captured = sh_secs(2026, 5, 26, 9, 59, 50);
        let (f, eligibility) = derive_freshness(&ctx, FreshnessIntent::Detail, td, captured, "tdx");
        assert!(eligibility.is_none());
        assert_eq!(f.status, FreshnessStatus::Fresh);
    }

    #[test]
    fn intraday_stale_over_threshold() {
        let now = sh(2026, 5, 26, 10, 0);
        let ctx = crate::domain::shared::resolve_market_time(now);
        let td = TradeDate::parse("20260526").unwrap();
        let captured = sh(2026, 5, 26, 9, 58); // 2 min ago
        let (f, eligibility) = derive_freshness(&ctx, FreshnessIntent::Detail, td, captured, "tdx");
        assert!(eligibility.is_none());
        assert_eq!(f.status, FreshnessStatus::Stale);
    }

    #[test]
    fn intraday_hard_expire_emits_missing() {
        let now = sh(2026, 5, 26, 14, 0);
        let ctx = crate::domain::shared::resolve_market_time(now);
        let td = TradeDate::parse("20260526").unwrap();
        let captured = sh(2026, 5, 26, 10, 0); // 4h ago
        let (f, eligibility) = derive_freshness(&ctx, FreshnessIntent::Detail, td, captured, "tdx");
        assert_eq!(f.status, FreshnessStatus::Missing);
        assert_eq!(eligibility, Some(WarningCode::SnapshotExpired));
    }

    #[test]
    fn off_session_close_fact_does_not_stale_by_age() {
        let now = sh(2026, 5, 26, 20, 0); // after close
        let ctx = crate::domain::shared::resolve_market_time(now);
        let td = TradeDate::parse("20260526").unwrap();
        let captured = sh(2026, 5, 26, 15, 5);
        let (f, eligibility) = derive_freshness(&ctx, FreshnessIntent::Universe, td, captured, "tdx");
        assert!(eligibility.is_none());
        assert_eq!(f.status, FreshnessStatus::Fresh);
    }

    #[test]
    fn off_session_wrong_trade_date_is_snapshot_expired() {
        let now = sh(2026, 5, 26, 20, 0);
        let ctx = crate::domain::shared::resolve_market_time(now);
        // 老 quote 对应前一天
        let td = TradeDate::parse("20260525").unwrap();
        let captured = sh(2026, 5, 25, 15, 5);
        let (f, eligibility) = derive_freshness(&ctx, FreshnessIntent::Detail, td, captured, "tdx");
        assert_eq!(f.status, FreshnessStatus::Missing);
        assert_eq!(eligibility, Some(WarningCode::SnapshotExpired));
    }

    #[test]
    fn intraday_wrong_trade_date_returns_quote_missing() {
        let now = sh(2026, 5, 26, 10, 0); // 交易时段
        let ctx = crate::domain::shared::resolve_market_time(now);
        let td = TradeDate::parse("20260525").unwrap(); // 老 quote
        let captured = sh(2026, 5, 25, 15, 5);
        let (f, eligibility) = derive_freshness(&ctx, FreshnessIntent::Detail, td, captured, "tdx");
        assert_eq!(f.status, FreshnessStatus::Missing);
        assert_eq!(eligibility, Some(WarningCode::QuoteMissing));
    }

    #[test]
    fn freshness_intent_threshold_default_values() {
        assert_eq!(FreshnessIntent::Detail.stale_threshold_secs(), 30);
        assert_eq!(FreshnessIntent::Universe.stale_threshold_secs(), 90);
    }

    #[test]
    fn universe_intent_uses_90s_threshold() {
        let now = sh(2026, 5, 26, 10, 0);
        let ctx = crate::domain::shared::resolve_market_time(now);
        let td = TradeDate::parse("20260526").unwrap();
        // 60s ago — 90s threshold → 仍 Fresh
        let captured = sh_secs(2026, 5, 26, 9, 59, 0);
        let (f, _) = derive_freshness(&ctx, FreshnessIntent::Universe, td, captured, "tdx");
        assert_eq!(f.status, FreshnessStatus::Fresh);
        // 同样 60s ago — Detail 30s threshold → Stale
        let (f2, _) = derive_freshness(&ctx, FreshnessIntent::Detail, td, captured, "tdx");
        assert_eq!(f2.status, FreshnessStatus::Stale);
    }
}
