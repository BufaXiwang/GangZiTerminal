//! Shared domain types — 跨 BC 公共契约。
//!
//! Spec: docs/design/shared-types.md
//!
//! 所有跨模块共享的类型只在此处定义；模块 spec 引用类型名，不重复定义。

pub mod codes;
pub mod events;
pub mod freshness;
pub mod market_time;
pub mod page;
pub mod types;

pub use codes::{ErrorCode, WarningCode};
pub use events::{AppEventEnvelope, JsonValue};
pub use freshness::{Freshness, FreshnessStatus};
pub use market_time::{resolve_market_time, MarketTimeContext};
pub use page::{ItemIssue, PageInfo, PageRequest};
pub use types::{
    Amount, InstrumentCategory, InstrumentStatus, Market, Money, OccurredAt, Percent, Price, Ratio,
    Shares, TimestampMs, TradeDate, TsCode, Volume,
};
