//! WarningCode / ErrorCode —— spec `shared-types.md §5`.
//!
//! 封闭共享集合；模块不能临时发明新机器可读 code，provider 原始错误 / 调试
//! 信息走 `message` / `details` / `payload`。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningCode {
    QuoteMissing,
    QuoteStale,
    SnapshotExpired,
    QuotePriceMissing,
    DepthMissing,
    InstrumentMissing,
    ProviderPartialFailure,
    ArticleMissing,
    QfqMissing,
    UsingUnadjustedKline,
    DailyBasicMissing,
    EventsMissing,
    StrategyOmitted,
    MappingMissing,
    DataPartial,
}

impl WarningCode {
    pub fn as_str(self) -> &'static str {
        match self {
            WarningCode::QuoteMissing => "quote_missing",
            WarningCode::QuoteStale => "quote_stale",
            WarningCode::SnapshotExpired => "snapshot_expired",
            WarningCode::QuotePriceMissing => "quote_price_missing",
            WarningCode::DepthMissing => "depth_missing",
            WarningCode::InstrumentMissing => "instrument_missing",
            WarningCode::ProviderPartialFailure => "provider_partial_failure",
            WarningCode::ArticleMissing => "article_missing",
            WarningCode::QfqMissing => "qfq_missing",
            WarningCode::UsingUnadjustedKline => "using_unadjusted_kline",
            WarningCode::DailyBasicMissing => "daily_basic_missing",
            WarningCode::EventsMissing => "events_missing",
            WarningCode::StrategyOmitted => "strategy_omitted",
            WarningCode::MappingMissing => "mapping_missing",
            WarningCode::DataPartial => "data_partial",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidInput,
    NotFound,
    ProviderUnavailable,
    RateLimited,
    DbError,
    ParseError,
    QuoteMissing,
    QuoteStale,
    QuotePriceMissing,
    DepthMissing,
    OutsideTradingSession,
    InstrumentNotTradable,
    InstrumentSuspended,
    LimitUpDownBlocked,
    InsufficientCash,
    InsufficientSellableQuantity,
    InvalidLotSize,
    OrderNotPending,
    RiskLimitExceeded,
    StrategyRequired,
    DuplicateEvent,
    VersionConflict,
    ArticleExtractFailed,
    ToolTimeout,
    ProviderContextTooLong,
}

impl ErrorCode {
    /// 字符串 → enum；用于 adapter / DB 反序列化以及把内部 spec 字符串收敛到 enum。
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "invalid_input" => Self::InvalidInput,
            "not_found" => Self::NotFound,
            "provider_unavailable" => Self::ProviderUnavailable,
            "rate_limited" => Self::RateLimited,
            "db_error" => Self::DbError,
            "parse_error" => Self::ParseError,
            "quote_missing" => Self::QuoteMissing,
            "quote_stale" => Self::QuoteStale,
            "quote_price_missing" => Self::QuotePriceMissing,
            "depth_missing" => Self::DepthMissing,
            "outside_trading_session" => Self::OutsideTradingSession,
            "instrument_not_tradable" => Self::InstrumentNotTradable,
            "instrument_suspended" => Self::InstrumentSuspended,
            "limit_up_down_blocked" => Self::LimitUpDownBlocked,
            "insufficient_cash" => Self::InsufficientCash,
            "insufficient_sellable_quantity" => Self::InsufficientSellableQuantity,
            "invalid_lot_size" => Self::InvalidLotSize,
            "order_not_pending" => Self::OrderNotPending,
            "risk_limit_exceeded" => Self::RiskLimitExceeded,
            "strategy_required" => Self::StrategyRequired,
            "duplicate_event" => Self::DuplicateEvent,
            "version_conflict" => Self::VersionConflict,
            "article_extract_failed" => Self::ArticleExtractFailed,
            "tool_timeout" => Self::ToolTimeout,
            "provider_context_too_long" => Self::ProviderContextTooLong,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidInput => "invalid_input",
            ErrorCode::NotFound => "not_found",
            ErrorCode::ProviderUnavailable => "provider_unavailable",
            ErrorCode::RateLimited => "rate_limited",
            ErrorCode::DbError => "db_error",
            ErrorCode::ParseError => "parse_error",
            ErrorCode::QuoteMissing => "quote_missing",
            ErrorCode::QuoteStale => "quote_stale",
            ErrorCode::QuotePriceMissing => "quote_price_missing",
            ErrorCode::DepthMissing => "depth_missing",
            ErrorCode::OutsideTradingSession => "outside_trading_session",
            ErrorCode::InstrumentNotTradable => "instrument_not_tradable",
            ErrorCode::InstrumentSuspended => "instrument_suspended",
            ErrorCode::LimitUpDownBlocked => "limit_up_down_blocked",
            ErrorCode::InsufficientCash => "insufficient_cash",
            ErrorCode::InsufficientSellableQuantity => "insufficient_sellable_quantity",
            ErrorCode::InvalidLotSize => "invalid_lot_size",
            ErrorCode::OrderNotPending => "order_not_pending",
            ErrorCode::RiskLimitExceeded => "risk_limit_exceeded",
            ErrorCode::StrategyRequired => "strategy_required",
            ErrorCode::DuplicateEvent => "duplicate_event",
            ErrorCode::VersionConflict => "version_conflict",
            ErrorCode::ArticleExtractFailed => "article_extract_failed",
            ErrorCode::ToolTimeout => "tool_timeout",
            ErrorCode::ProviderContextTooLong => "provider_context_too_long",
        }
    }
}
