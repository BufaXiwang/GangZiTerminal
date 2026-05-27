//! Quotes 跨模块事件 payload。
//!
//! Spec: docs/design/quotes-module.md §5；shared-types.md §6

use crate::domain::shared::{OccurredAt, TradeDate, TsCode};
use serde::{Deserialize, Serialize};
use specta::Type;

/// Spec: shared-types.md §6 `MarketQuotesRefreshedPayload`
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct MarketQuotesRefreshedPayload {
    pub scope: RefreshScope,
    pub purpose: RefreshPurpose,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trade_date: Option<TradeDate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affected_ts_codes: Option<Vec<TsCode>>,
    pub total: u32,
    pub success: u32,
    pub failed_batches: u32,
    pub captured_at: OccurredAt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum RefreshScope {
    Subscribed,
    Universe,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum RefreshPurpose {
    Intraday,
    Close,
}

/// Tauri event channel name.
pub const MARKET_QUOTES_REFRESHED_EVENT: &str = "market-quotes-refreshed";
