//! News 域纯类型——NewsItem（资讯条目）/ ArticleContent（正文抽取结果）。
//!
//! 这两个类型同时跨"infrastructure（抓取/落 DB）"和"agent prompt（search_news 工具
//! 返回 JSON）"，因此放在 domain 层避免双向依赖。

use serde::{Deserialize, Serialize};

use super::NewsError;

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

/// News processing state for future briefing/review pipelines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewsStatus {
    Pending,
    Processing,
    Consumed,
    Failed,
}

impl NewsStatus {
    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Pending, Self::Processing)
                | (Self::Processing, Self::Consumed)
                | (Self::Processing, Self::Failed)
                | (Self::Processing, Self::Pending)
                | (Self::Failed, Self::Pending)
        )
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
    #[serde(
        default,
        rename = "analysisStatus",
        skip_serializing_if = "Option::is_none"
    )]
    pub analysis_status: Option<NewsStatus>,
}

impl NewsItem {
    pub fn status_or_pending(&self) -> NewsStatus {
        self.analysis_status.unwrap_or(NewsStatus::Pending)
    }

    pub fn transition_to(&mut self, next: NewsStatus) -> Result<(), NewsError> {
        let current = self.status_or_pending();
        if current.can_transition_to(next) || current == next {
            self.analysis_status = Some(next);
            Ok(())
        } else {
            Err(NewsError::InvalidState(format!(
                "news {} 状态不能从 {:?} 转到 {:?}",
                self.id, current, next
            )))
        }
    }
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
