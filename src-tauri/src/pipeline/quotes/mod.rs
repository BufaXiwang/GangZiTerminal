//! Quotes pipeline — use case + 后台任务。
//!
//! Spec: docs/design/quotes-module.md §3 / §4 / §5
//!
//! 铁律：pipeline/quotes 不依赖 adapters。

pub mod facade;
pub mod scheduler;
pub mod service;

pub use facade::{get_quote_snapshot, get_quote_snapshots};
pub use scheduler::{spawn_quotes_scheduler, QuotesSchedulerHandle};
pub use service::{QuotesService, QuotesServiceConfig};
