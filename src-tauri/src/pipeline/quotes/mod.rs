//! Quotes pipeline — use case + 后台任务。
//!
//! Spec: docs/design/quotes-module.md §3 / §4 / §5
//!
//! 铁律：pipeline/quotes 不依赖 adapters。

pub mod facade;
pub mod market_time;
pub mod scheduler;
pub mod service;

/// 实网集成测试（#[ignore]，不在普通 cargo test 运行）。
#[cfg(test)]
mod live_integration_tests;

pub use facade::{get_quote_snapshot, get_quote_snapshots};
pub use market_time::resolve_market_time_with_calendar;
pub use scheduler::{spawn_quotes_scheduler, QuotesSchedulerHandle};
pub use service::{QuotesService, QuotesServiceConfig};
