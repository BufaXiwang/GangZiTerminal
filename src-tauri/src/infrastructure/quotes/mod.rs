//! Quotes infrastructure — SQLite + provider adapters + cache。
//!
//! Spec: docs/design/quotes-module.md §3 / §5
//!
//! 铁律：infrastructure/quotes 不允许 use pipeline | adapters。

pub mod migrations;
pub mod snapshot_cache;
pub mod tdx;
pub mod trade_calendar;

pub use migrations::migrations;
pub use snapshot_cache::{CachedSnapshot, SnapshotCache};
pub use trade_calendar::{TradeCalendar, TradeCalendarRepo};

// 后续 sub-agent 实现 provider adapter（eastmoney / sina / tencent / tushare）
// 和 repository 时在此处补 `pub mod ...` 声明。
