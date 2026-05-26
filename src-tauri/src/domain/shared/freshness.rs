//! Freshness 状态。
//!
//! Spec: docs/design/shared-types.md §4

use super::{codes::WarningCode, types::OccurredAt};
use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum FreshnessStatus {
    Fresh,
    Stale,
    Missing,
}

/// Spec: shared-types.md §4
/// - `missing` 表示本地读模型没有可用数据，或数据已超过模块定义的硬过期阈值而不可再作为可用事实返回。
/// - `stale` 表示本地有数据，但不满足当前用途的新鲜度要求。
/// - Account 交易写路径必须 fail closed：`stale` / `missing` quote 不得成交。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
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
