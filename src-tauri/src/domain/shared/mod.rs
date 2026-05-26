#![allow(dead_code, unused_imports)] // newtype 提供完整 ctor / accessor 套件

//! Domain `shared` —— 跨 bounded context 复用的 newtype / value object。
//!
//! 对齐 docs/design/shared-types.md：所有 BC 共享语义在此一处定义，模块不
//! 重复声明。任何 crate::* 内部模块都可以 `use crate::domain::shared::*`。

pub mod board;
pub mod codes;
pub mod events;
pub mod freshness;
pub mod ids;
pub mod market_time;
pub mod money;
pub mod shares;
pub mod signal;
pub mod time;

pub use board::{classify as classify_board, Board};
pub use codes::{ErrorCode, WarningCode};
pub use events::{
    AccountTriggerKind, AccountTriggeredPayload, AccountUpdatedPayload,
    MarketQuotesPurpose, MarketQuotesRefreshedPayload, MarketQuotesScopeKind,
    NewsFailure, NewsRefreshWarning, NewsRefreshedPayload, NewsStage,
};
pub use freshness::{Freshness, FreshnessStatus};
pub use ids::{IdError, StockCode, TsCode};
pub use market_time::{resolve_market_time, MarketTimeContext};
pub use money::{KYuan, MoneyError, Yuan};
pub use shares::{Lots, Shares, SharesError};
pub use signal::{EventKind, SignalDetection, SignalKind};
pub use time::{OccurredAt, TimeError, TradeDate};
