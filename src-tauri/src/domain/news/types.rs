//! News 域纯类型——NewsItem（资讯条目）/ ArticleContent（正文抽取结果）。
//!
//! 这两个类型同时跨"infrastructure（抓取/落 DB）"和"agent prompt（search_news 工具
//! 返回 JSON）"，因此放在 domain 层避免双向依赖。

use serde::{Deserialize, Serialize};

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
///
/// `paragraphs` 按段切分（保留段内格式），`images` 是绝对 URL 列表。
/// `extraction` 标记哪个 extractor 给出的结果（debug / 复盘选 source 时用）。
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
