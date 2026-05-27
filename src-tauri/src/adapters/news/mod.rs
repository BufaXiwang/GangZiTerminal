//! News BC adapters — Tauri commands + 事件 type 常量。
//!
//! Spec: docs/design/news-module.md §4 / §5（依赖约束：adapters 只做 IPC DTO 转换）

pub mod cmd;
pub mod events;

pub use cmd::{fetch_news, list_news_sources, warm_articles};
pub use events::NEWS_REFRESHED_EVENT;
