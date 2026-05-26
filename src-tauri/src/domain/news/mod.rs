//! Domain `news`——资讯抓取 / 正文抽取的 Bounded Context。
//!
//! 纯类型 + 错误，无 I/O。
//! - 抓取实现见 `infrastructure::news`
//! - 编排见 `pipeline::news`
//! - IPC 见 `adapters::news_commands`

pub mod canonical_url;
pub mod errors;
pub mod types;

pub use errors::NewsError;
pub use types::{ArticleContent, NewsItem};
