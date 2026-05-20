//! Pipeline `news`——资讯刷新 use case 编排层。
//!
//! 调 infrastructure::news 的 fetchers 拉数据 + 写 SQLite 落盘 + emit 事件。
//! Tauri command 入口在 adapters/news_commands.rs（薄包装）。

pub mod refresh;

pub use refresh::{run_news_refresh, NewsRefreshResult};
