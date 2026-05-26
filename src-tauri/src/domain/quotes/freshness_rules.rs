//! Quote 有效性规则 —— spec `quotes-module.md §2`。
//!
//! - 交易时段内：使用 `tradeDate == currentTradeDate` 的 quote，且 `now - capturedAt`
//!   不超过 1h 硬过期；超过则 `freshness.status = missing`，warning = snapshot_expired。
//! - 非交易时段：使用 `tradeDate == latestCompletedTradeDate` 的 quote；不因 1h 过期。
//! - tradeDate 不匹配 eligible trade date → snapshot_expired / quote_missing。
//!
//! 该 helper 被 `adapters/quotes_canonical::fetch_data` 与 `adapters/agent_tools::
//! fetch_quotes` 共用，避免两套口径。

use crate::domain::quotes::StockQuote;
use crate::domain::shared::market_time::MarketTimeContext;
use crate::domain::shared::{Freshness, FreshnessStatus, WarningCode};

pub const HARD_EXPIRY_MS: i64 = 3_600_000;

#[derive(Debug, Clone)]
pub struct ResolvedQuote<'a> {
    /// 仍然可展示的 quote；硬过期或 tradeDate 不匹配时为 None。
    pub quote: Option<&'a StockQuote>,
    /// 派生 freshness（永远 Some）。
    pub freshness: Freshness,
    /// 派生 warning code（若需附加到 item warning 数组）。
    pub warning: Option<WarningCode>,
}

/// spec quotes-module.md §2「五档盘口缺失返回 depth_missing」。集中判定 helper，
/// 避免各 adapter 各自手写 `bid_levels.is_empty() || ask_levels.is_empty()`。
///
/// 返回 None 表示不缺失；Some(WarningCode::DepthMissing) 表示需要附 warning。
/// 缺失条件：
/// - bid_levels 或 ask_levels 整体为空
/// - 一档（bid[0] / ask[0]）价格 Some 但为 0 / 缺失
pub fn validate_depth_levels(q: &StockQuote) -> Option<WarningCode> {
    if q.bid_levels.is_empty() || q.ask_levels.is_empty() {
        return Some(WarningCode::DepthMissing);
    }
    let bid_top_price = q.bid_levels.first().and_then(|l| l.price.as_ref());
    let ask_top_price = q.ask_levels.first().and_then(|l| l.price.as_ref());
    if bid_top_price.is_none() || ask_top_price.is_none() {
        return Some(WarningCode::DepthMissing);
    }
    None
}

pub fn resolve_quote_view<'a>(
    snapshot: Option<&'a StockQuote>,
    mkt: &MarketTimeContext,
    now_ms: i64,
) -> ResolvedQuote<'a> {
    let Some(q) = snapshot else {
        return ResolvedQuote {
            quote: None,
            freshness: Freshness::missing(WarningCode::QuoteMissing),
            warning: Some(WarningCode::QuoteMissing),
        };
    };
    let eligible = if mkt.is_trading_time {
        mkt.current_trade_date.as_ref().map(|d| d.to_compact())
    } else {
        Some(mkt.latest_completed_trade_date.to_compact())
    };
    let snap_td = q.trade_date.to_compact();
    if eligible.as_deref() != Some(snap_td.as_str()) {
        return ResolvedQuote {
            quote: None,
            freshness: Freshness::missing(WarningCode::SnapshotExpired),
            warning: Some(WarningCode::SnapshotExpired),
        };
    }
    if mkt.is_trading_time && now_ms - q.captured_at.value() > HARD_EXPIRY_MS {
        return ResolvedQuote {
            quote: None,
            freshness: Freshness::missing(WarningCode::SnapshotExpired),
            warning: Some(WarningCode::SnapshotExpired),
        };
    }
    let warning = match q.freshness.status {
        FreshnessStatus::Stale => Some(WarningCode::QuoteStale),
        FreshnessStatus::Missing => Some(WarningCode::QuoteMissing),
        FreshnessStatus::Fresh => None,
    };
    ResolvedQuote {
        quote: Some(q),
        freshness: q.freshness.clone(),
        warning,
    }
}
