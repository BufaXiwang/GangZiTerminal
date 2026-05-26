//! 跨 BC 应用事件 payload —— spec `shared-types.md §6`.
//!
//! 这些类型是 News / Account / Quotes / Agent Runtime 之间发布订阅的
//! contract。生产者只发布事实，不指定消费者。消费者按 event key 做幂等
//! 处理。
//!
//! 每个 payload 都是 **camelCase** 序列化，匹配 spec 的 TypeScript 定义；
//! Rust 内部读取统一用 snake_case 字段。

use serde::{Deserialize, Serialize};

use super::codes::{ErrorCode, WarningCode};
use super::freshness::Freshness;

/// 资讯刷新阶段。spec `NewsFailure.stage` / `NewsRefreshWarning.stage`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewsStage {
    Fetch,
    Normalize,
    Save,
    Article,
}

/// 单条 provider 失败事实。spec `NewsFailure`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewsFailure {
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub code: ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<NewsStage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    pub occurred_at: String,
}

/// 单条 provider 警告事实。spec `NewsRefreshWarning`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewsRefreshWarning {
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub code: WarningCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<NewsStage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_count: Option<usize>,
    pub occurred_at: String,
}

/// `news-refreshed` 事件 payload。spec `NewsRefreshedPayload`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewsRefreshedPayload {
    pub batch_id: String,
    pub fetched_count: usize,
    pub skipped_count: usize,
    pub saved_count: usize,
    pub article_updated_count: usize,
    pub new_ids: Vec<String>,
    pub updated_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub article_updated_news_ids: Option<Vec<String>>,
    pub failed_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_failure: Option<NewsFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failures: Option<Vec<NewsFailure>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Vec<NewsRefreshWarning>>,
}

/// 行情刷新范围。spec `MarketQuotesRefreshedPayload.scope`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketQuotesScopeKind {
    Subscribed,
    Universe,
    Manual,
}

/// 行情刷新目的。spec `MarketQuotesRefreshedPayload.purpose`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketQuotesPurpose {
    Intraday,
    Close,
}

/// `market-quotes-refreshed` 事件 payload。spec `MarketQuotesRefreshedPayload`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketQuotesRefreshedPayload {
    pub scope: MarketQuotesScopeKind,
    pub purpose: MarketQuotesPurpose,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trade_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affected_ts_codes: Option<Vec<String>>,
    pub total: usize,
    pub success: usize,
    pub failed_batches: usize,
    pub captured_at: String,
}

/// `account-updated` 事件 payload。spec `AccountUpdatedPayload`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountUpdatedPayload {
    pub account_event_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affected_order_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affected_position_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affected_ts_codes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affected_watchlist_ts_codes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_ids: Option<Vec<String>>,
    pub snapshot_captured_at: String,
}

/// 触发类型。spec `AccountTriggeredPayload.triggerType`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountTriggerKind {
    StopLoss,
    TakeProfit,
    TimeStop,
    OrderFilled,
    OrderRejected,
    OrderExpired,
    Invalidated,
}

/// `account-triggered` 事件 payload。spec `AccountTriggeredPayload`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountTriggeredPayload {
    pub trigger_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts_code: Option<String>,
    pub trigger_type: AccountTriggerKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_freshness: Option<Freshness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Vec<WarningCode>>,
}
