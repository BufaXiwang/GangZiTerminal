//! News 域纯类型——NewsItem（资讯条目）/ ArticleContent（正文抽取结果）。
//!
//! News 模块只提供"拉取 + 存储 + 查询"。任何分析 / 消费状态由 Agent BC 自管
//! （见 `domain::agent::news_analysis::NewsAnalysisStatus`）。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NewsId(String);

impl NewsId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 一条资讯（标题 + 元信息）。
///
/// `id` 由 fetcher 决定唯一性策略：
/// - NewsNow: `{source_id}-{item.id|link|index-title}`
/// - RSS: guid → link → 兜底 `{source}-{index}-{title}`
///
/// `summary` 来自 RSS description 或 NewsNow extra.hover，可能为空。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsItem {
    pub id: String,
    pub title: String,
    pub link: Option<String>,
    pub source: String,
    pub published: Option<String>,
    pub summary: Option<String>,
}

/// 一篇资讯的正文抽取结果——`fetch_article_content` Tauri command 返回，
/// 同时进 SQLite `article_contents` 表做缓存。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArticleContent {
    pub url: String,
    pub title: String,
    pub source: Option<String>,
    pub published: Option<String>,
    pub author: Option<String>,
    pub paragraphs: Vec<String>,
    pub images: Vec<String>,
    pub fetched_at: String,
    pub extraction: String,
}
