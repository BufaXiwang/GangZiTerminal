#![allow(dead_code)] // NewsError variants 按需展开供 Step B 后续调用方使用

//! Domain `news`——资讯抓取 / 正文抽取的 Bounded Context。
//!
//! 纯类型 + 错误，无 I/O。
//! - 抓取实现见 `infrastructure::news`
//! - 编排见 `pipeline::news`
//! - IPC 见 `adapters::news_commands`

pub mod errors;
pub mod types;

pub use errors::NewsError;
pub use types::{ArticleContent, NewsItem};
