//! TuShare Pro adapter — universe enrich / 历史 K / 复权 / daily_basic / 公司事件 / 交易日历。
//!
//! Spec: docs/design/quotes-module.md §5；docs/design/references/quotes/tushare.md

pub mod calendar;
pub mod client;
pub mod events;
pub mod kline;
pub mod universe;

pub use calendar::CalendarEntry;
pub use client::{TushareClient, TushareError};
