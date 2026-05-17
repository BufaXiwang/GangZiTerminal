//! Infrastructure `news`——资讯抓取 + 正文抽取的 I/O 实现。
//!
//! 提供两个能力：
//! - **fetchers**（`newsnow` / `rss`）：从远端拉一批资讯条目
//! - **article**：拉具体一篇正文 + 抽段（缓存进 SQLite `article_contents` 表）
//!
//! domain 层（`crate::domain::news`）只定义类型和错误；这里负责把外部 HTTP / 解析
//! 错误 map 成 `NewsError`。

pub mod article;
pub mod newsnow;
pub mod repository;
pub mod rss;

mod util;

pub use newsnow::fetch_newsnow_source;
pub use rss::fetch_rss;
// `article::fetch_article_remote` 直接走完整路径调用，无须 re-export
