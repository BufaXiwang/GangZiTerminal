//! News pipeline — application use cases / 后台编排。
//!
//! Spec: docs/design/news-module.md §3 / §5（依赖约束：不依赖 adapters）

pub mod scheduler;
pub mod service;

pub use scheduler::{spawn_news_refresh_scheduler, NewsSchedulerHandle};
pub use service::{NewsRefreshedEvent, NewsService};
