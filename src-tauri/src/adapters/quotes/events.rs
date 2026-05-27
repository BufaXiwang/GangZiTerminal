//! Quotes 对外事件 envelope helper。
//!
//! Spec: docs/design/quotes-module.md §5；shared-types.md §6

use crate::domain::quotes::MarketQuotesRefreshedPayload;
use crate::domain::shared::AppEventEnvelope;
use chrono::Utc;
use uuid::Uuid;

pub use crate::domain::quotes::MARKET_QUOTES_REFRESHED_EVENT;

pub fn wrap_market_quotes_refreshed(
    payload: MarketQuotesRefreshedPayload,
    correlation_id: Option<String>,
) -> AppEventEnvelope<MarketQuotesRefreshedPayload> {
    AppEventEnvelope {
        event_id: Uuid::new_v4().to_string(),
        event_type: MARKET_QUOTES_REFRESHED_EVENT.to_string(),
        occurred_at: Utc::now(),
        correlation_id,
        causation_id: None,
        payload,
    }
}
