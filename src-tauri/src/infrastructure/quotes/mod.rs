//! Quotes infrastructure — SQLite + provider adapters + cache。
//!
//! Spec: docs/design/quotes-module.md §3 / §5
//!
//! 铁律：infrastructure/quotes 不允许 use pipeline | adapters。

pub mod config;
pub mod eastmoney;
pub mod migrations;
pub mod repository;
pub mod seed;
pub mod sina;
pub mod snapshot_cache;
pub mod tdx;
pub mod tencent;
pub mod trade_calendar;
pub mod tushare;
pub mod universe;

pub use config::QuotesConfig;
pub use eastmoney::EastmoneyProvider;
pub use migrations::migrations;
pub use repository::QuotesRepository;
pub use seed::seed_builtin_instruments;
pub use sina::SinaProvider;
pub use snapshot_cache::{CachedSnapshot, SnapshotCache};
pub use tdx::{TdxConnectionManager, TdxManagerError};
pub use tencent::TencentProvider;
pub use trade_calendar::{TradeCalendar, TradeCalendarRepo, WeekdayCalendar};
pub use tushare::{CalendarEntry, TushareClient};
