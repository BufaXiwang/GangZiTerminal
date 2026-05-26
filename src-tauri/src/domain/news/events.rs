//! News refresh 事件 payload。
//!
//! Spec: docs/design/shared-types.md §6 (NewsRefreshedPayload / NewsFailure / NewsRefreshWarning)
//! Spec: docs/design/news-module.md §5 (failure code 规则)

use crate::domain::shared::{ErrorCode, JsonValue, OccurredAt, WarningCode};
use serde::{Deserialize, Serialize};
use specta::Type;

/// Refresh stage（spec §4 / §5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum NewsRefreshStage {
    Fetch,
    Normalize,
    Save,
    Article,
}

/// 单 provider / source 失败（shared-types.md §6 NewsFailure）。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NewsFailure {
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub code: ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<NewsRefreshStage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    pub occurred_at: OccurredAt,
}

/// 单条 item 跳过 / partial 提示（shared-types.md §6 NewsRefreshWarning）。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NewsRefreshWarning {
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub code: WarningCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<NewsRefreshStage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_count: Option<u32>,
    pub occurred_at: OccurredAt,
}

/// Refresh 完成事件 payload（shared-types.md §6）。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NewsRefreshedPayload {
    pub batch_id: String,
    pub fetched_count: u32,
    pub skipped_count: u32,
    pub saved_count: u32,
    pub article_updated_count: u32,
    pub new_ids: Vec<String>,
    pub updated_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub article_updated_news_ids: Vec<String>,
    pub failed_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_failure: Option<NewsFailure>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<NewsFailure>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<NewsRefreshWarning>,
}
