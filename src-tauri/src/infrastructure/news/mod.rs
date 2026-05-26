//! News BC infrastructure — DB / HTTP provider 实现。
//!
//! Spec: docs/design/news-module.md §5 (依赖约束：可依赖 DB / HTTP，不依赖 pipeline / adapters)

pub mod article_extractor;
pub mod migrations;
pub mod newsnow;
pub mod registry;
pub mod repository;
pub mod rss;

pub use migrations::migrations;
pub use registry::SourceRegistry;
pub use repository::{NewsRepository, RepoArticleUpsertOutcome, RepoItemUpsertOutcome};
