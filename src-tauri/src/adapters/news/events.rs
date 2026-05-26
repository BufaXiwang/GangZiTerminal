//! News BC 对外事件常量 + envelope helper。
//!
//! Spec: docs/design/news-module.md §5；shared-types.md §6（AppEventEnvelope）

use crate::domain::news::events::NewsRefreshedPayload;
use crate::domain::shared::AppEventEnvelope;
use chrono::Utc;
use uuid::Uuid;

/// 前端 `listen('news-refreshed', ...)` 使用的事件类型。
pub const NEWS_REFRESHED_EVENT: &str = "news-refreshed";

pub fn wrap_news_refreshed(
    payload: NewsRefreshedPayload,
    correlation_id: Option<String>,
) -> AppEventEnvelope<NewsRefreshedPayload> {
    AppEventEnvelope {
        event_id: Uuid::new_v4().to_string(),
        event_type: NEWS_REFRESHED_EVENT.to_string(),
        occurred_at: Utc::now(),
        correlation_id,
        causation_id: None,
        payload,
    }
}
