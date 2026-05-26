//! `Freshness` —— spec `shared-types.md §4`.
//!
//! 跨模块共享的新鲜度元数据。Quotes 派生它给 query facade，Account 和
//! Agent 消费它判断是否可作为当前交易事实。

use serde::{Deserialize, Serialize};

use super::codes::WarningCode;
use super::time::OccurredAt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessStatus {
    Fresh,
    Stale,
    Missing,
}

impl Default for FreshnessStatus {
    fn default() -> Self {
        FreshnessStatus::Missing
    }
}

impl Default for Freshness {
    fn default() -> Self {
        Self {
            status: FreshnessStatus::Missing,
            captured_at: None,
            exchange_time: None,
            age_ms: None,
            source: None,
            warning: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Freshness {
    pub status: FreshnessStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<OccurredAt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exchange_time: Option<OccurredAt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<WarningCode>,
}

impl Freshness {
    pub fn missing(warning: WarningCode) -> Self {
        Self {
            status: FreshnessStatus::Missing,
            captured_at: None,
            exchange_time: None,
            age_ms: None,
            source: None,
            warning: Some(warning),
        }
    }

    pub fn fresh(captured_at: OccurredAt, source: impl Into<String>) -> Self {
        Self {
            status: FreshnessStatus::Fresh,
            captured_at: Some(captured_at),
            exchange_time: None,
            age_ms: None,
            source: Some(source.into()),
            warning: None,
        }
    }

    pub fn stale(captured_at: OccurredAt, age_ms: i64, source: impl Into<String>) -> Self {
        Self {
            status: FreshnessStatus::Stale,
            captured_at: Some(captured_at),
            exchange_time: None,
            age_ms: Some(age_ms),
            source: Some(source.into()),
            warning: Some(WarningCode::QuoteStale),
        }
    }
}
