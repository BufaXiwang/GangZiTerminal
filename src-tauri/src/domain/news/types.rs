//! News domain 类型 + 对外 DTO。
//!
//! Spec: docs/design/news-module.md §2 / §4

use crate::domain::shared::{JsonValue, OccurredAt, WarningCode};
use serde::{Deserialize, Serialize};
use specta::Type;

use super::events::{NewsRefreshWarning, NewsFailure};
use super::errors::WarmArticlesError;
use super::source::NewsSource;

// ============================================================================
// §2 领域模型
// ============================================================================

/// 一条资讯主记录。`payload` 保存 provider 原始信息（spec §2）。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NewsItem {
    pub id: String,
    pub source: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<OccurredAt>,
    pub payload: JsonValue,
    pub created_at: OccurredAt,
    pub updated_at: OccurredAt,
}

/// 某 canonical URL 的正文抽取结果（spec §2）。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ArticleContent {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_news_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    pub payload: JsonValue,
    pub fetched_at: OccurredAt,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<WarningCode>,
}

/// Provider 标准化输出（spec §5 Provider 策略）。
#[derive(Debug, Clone)]
pub struct ProviderNewsItem {
    pub id: String,
    pub source: String,
    pub title: String,
    pub summary: Option<String>,
    pub url: Option<String>,
    pub published_at: Option<OccurredAt>,
    pub payload: JsonValue,
}

// ============================================================================
// §4 对外接口 — fetch_news
// ============================================================================

#[derive(Debug, Clone, Deserialize, Serialize, Type, Default)]
#[serde(rename_all = "camelCase")]
pub struct FetchNewsRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_from: Option<OccurredAt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_to: Option<OccurredAt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_article: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ArticleSnippet {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<OccurredAt>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type, Default)]
#[serde(rename_all = "camelCase")]
pub struct NewsItemFreshness {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub article_fetched_at: Option<OccurredAt>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FetchNewsItem {
    pub id: String,
    pub source: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<OccurredAt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub article_excerpt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub article: Option<ArticleSnippet>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<NewsItemFreshness>,
    // Spec / binding 把 warnings & errors 标为必填 (`WarningCode[]` / `ErrorCode[]`)；
    // 不能 skip-if-empty，否则前端拿到 undefined.length 直接崩。空时必须送 `[]`。
    #[serde(default)]
    pub warnings: Vec<WarningCode>,
    #[serde(default)]
    pub errors: Vec<crate::domain::shared::ErrorCode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FetchNewsError {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub code: crate::domain::shared::ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FetchNewsPage {
    pub limit: u32,
    pub offset: u32,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FetchNewsResponse {
    pub items: Vec<FetchNewsItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<FetchNewsError>,
    pub page: FetchNewsPage,
    /// 按北京日期(YYYY-MM-DD)的每日真实总条数（同 filter，不受分页限制）。
    /// 资讯页日期导航用它显示每天真实数量，而非分页累积。
    #[serde(default)]
    pub date_counts: Vec<NewsDateCount>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NewsDateCount {
    pub date: String,
    pub count: u32,
}

// ============================================================================
// list_news_sources
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ListNewsSourcesResponse {
    pub items: Vec<NewsSource>,
}

// ============================================================================
// Untagged ok/err flag helpers — 用于 warm_articles 等 `{ ok: true | false }` 响应形状。
// ============================================================================

/// 序列化为 JSON `true`。用于 untagged enum 的判别字段。
#[derive(Debug, Clone, Copy, Default)]
pub struct TrueFlag;

impl Serialize for TrueFlag {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bool(true)
    }
}

impl<'de> Deserialize<'de> for TrueFlag {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = bool::deserialize(d)?;
        if v {
            Ok(TrueFlag)
        } else {
            Err(serde::de::Error::custom("expected `ok: true`"))
        }
    }
}

impl specta::Type for TrueFlag {
    fn inline(_t: &mut specta::TypeMap, _g: specta::Generics) -> specta::DataType {
        specta::DataType::Literal(specta::datatype::LiteralType::bool(true))
    }
}

/// 序列化为 JSON `false`。
#[derive(Debug, Clone, Copy, Default)]
pub struct FalseFlag;

impl Serialize for FalseFlag {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bool(false)
    }
}

impl<'de> Deserialize<'de> for FalseFlag {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = bool::deserialize(d)?;
        if !v {
            Ok(FalseFlag)
        } else {
            Err(serde::de::Error::custom("expected `ok: false`"))
        }
    }
}

impl specta::Type for FalseFlag {
    fn inline(_t: &mut specta::TypeMap, _g: specta::Generics) -> specta::DataType {
        specta::DataType::Literal(specta::datatype::LiteralType::bool(false))
    }
}

// ============================================================================
// warm_articles
// ============================================================================

#[derive(Debug, Clone, Deserialize, Serialize, Type, Default)]
#[serde(rename_all = "camelCase")]
pub struct WarmArticlesRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub news_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct WarmArticlesResult {
    pub batch_id: String,
    pub requested_count: u32,
    pub attempted_count: u32,
    pub article_updated_count: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub article_updated_news_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<NewsRefreshWarning>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<NewsFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(untagged)]
pub enum WarmArticlesResponse {
    Ok(WarmArticlesOk),
    Err(WarmArticlesErr),
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct WarmArticlesOk {
    pub ok: TrueFlag,
    pub result: WarmArticlesResult,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct WarmArticlesErr {
    pub ok: FalseFlag,
    pub error: WarmArticlesError,
}
