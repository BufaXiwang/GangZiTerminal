//! Domain `quotes`——市场数据 Bounded Context。
//!
//! 这里只放**纯 domain 类型**——无 I/O、无外部网络、无 Tauri。
//! - `errors`：QuotesError 错误类型
//! - `types`：StockQuote / KlinePoint / scanner / events / calendar / fund 等 domain 数据结构
//! - `indicators`：MA / RSI / MACD / KDJ / CCI / BOLL / ATR / OBV 等 pure 计算
//! - `clock`：A 股交易时段判断
//!
//! I/O 实现在 `crate::infrastructure::*`——HTTP client 适配 TuShare / EM，SQLite
//! repository 适配本地缓存，snapshot 模块维护内存状态。
//!
//! 依赖单向（per architecture.md § 1.3）：
//! - quotes 模块**不**引用 agent / account / news / pipeline / infrastructure
//! - 只引用 `crate::domain::shared::*`

pub mod clock;
pub mod errors;
pub mod indicators;
pub mod types;

pub use clock::is_a_share_trading_hours;
pub use errors::QuotesError;
pub use indicators::{compute_indicators, IndicatorConfig, IndicatorSnapshot};
pub use types::{
    AdjMode, CompanyEvent, ConceptPerformance, ConceptSector, DailyBasic, ForecastType,
    HistorySource, InstrumentCategory, KlinePeriod, KlinePoint, KlineSeries, ListStatus,
    MarginSummary, MarketBreadth, MarketIndex, MarketInstrument, MarketOverview, MinuteKlinePoint,
    MinuteKlineSeries, MinutePeriod, MinutePoint, MoneyFlowItem, NorthHolding, NorthMoneyFlow,
    OrderBookLevel, ScanCondition, ScanFilter, ScanItem, ScanOp, ScanResult, ScanSort, StStatus,
    StockProfile, StockQuote, StockRef, TopListItem, TradeCalendar,
};
