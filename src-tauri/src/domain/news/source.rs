//! NewsSource — 资讯来源配置 + 健康状态。
//!
//! Spec: docs/design/news-module.md §2 (NewsSource)
//!
//! NewsSource 列表是 **编译期常量**（spec §2）：第一阶段不提供运行时配置入口、
//! 不提供管理 UI；新增 / 删除 / 修改 source 必须改 `infrastructure/news/registry.rs`
//! 中的 `DEFAULT_SOURCES` 并重新部署。`enabled` 字段同样在代码中固化。

use crate::domain::shared::{ErrorCode, OccurredAt};
use serde::{Deserialize, Serialize};
use specta::Type;

/// `source_id` 由 `namespace:channel` 形式构成；与 `NewsItem.source` 完全一致。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NewsSource {
    pub source_id: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dynamic: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh_at: Option<OccurredAt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<NewsSourceLastError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NewsSourceLastError {
    pub code: ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub occurred_at: OccurredAt,
}

/// 简明引用：用于 provider 调用时识别 source。
#[derive(Debug, Clone)]
pub struct NewsSourceRef {
    pub source_id: String,
    pub provider: String,
    pub feed_url: Option<String>,
    pub display_name: Option<String>,
    pub enabled: bool,
}
