//! Quotes 模块 domain。
//!
//! Spec: docs/design/quotes-module.md
//!
//! 纯类型 + 规则；不依赖 tauri / rusqlite / reqwest / tdx / infrastructure / pipeline / adapters。

pub mod adjust;
pub mod errors;
pub mod events;
pub mod freshness_rules;
pub mod indicators;
pub mod instrument;
pub mod kline;
pub mod limit;
pub mod quote;
pub mod scan;
pub mod sources;
pub mod trade_calendar;
pub mod tushare_health;
pub mod xdxr;

pub use adjust::{apply_adjust, AdjustMode};
pub use errors::{QuoteFacadeError, QuoteFacadeErrorKind};
pub use events::{
    MarketQuotesRefreshedPayload, RefreshDataScope, RefreshMarketQuotesScope, RefreshPurpose,
    RefreshScope, RefreshScopeKind,
};
pub use freshness_rules::{
    derive_freshness, eligible_trade_date, EligibleTradeDate, FreshnessIntent,
    DETAIL_STALE_THRESHOLD_SECS, HARD_EXPIRE_SECS, UNIVERSE_STALE_THRESHOLD_SECS,
};
pub use indicators::{compute_indicators, IndicatorBasis, IndicatorName, IndicatorSnapshot};
pub use instrument::{InstrumentSource, MarketInstrument, StockProfile};
pub use kline::{
    Adjust, IntradaySeries, KlinePeriod, KlinePoint, KlineSeries, MinuteKlinePeriod,
    MinuteKlinePoint, MinuteKlineSeries, MinutePoint,
};
pub use limit::{apply_band as apply_band_helper, compute_limit_band, LimitBand};
pub use quote::{
    CompanyEvent, CompanyEventType, DailyBasic, MarketQuoteSnapshot, QuoteDepthLevel, QuoteSource,
    StockQuote, TradeStatus,
};
pub use scan::{
    ScanCondition, ScanConditionField, ScanConditionValue, ScanCriteria, ScanFilter, ScanItem,
    ScanOp, ScanResult, ScanSortBy, ScanUniverse,
};
pub use sources::core_indexes;
pub use trade_calendar::{
    is_trading_day, next_trading_day, previous_trading_day, trading_days_between,
};
pub use tushare_health::{TushareHealthConfig, TushareHealthState};
pub use xdxr::{XdxrCategory, XdxrEvent};

// Spec: quotes-module.md §1 / §4
pub use events::MARKET_QUOTES_REFRESHED_EVENT;
