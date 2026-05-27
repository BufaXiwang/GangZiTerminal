//! News 模块错误类型 — DTO 级 error。
//!
//! Spec: docs/design/news-module.md §4 / §5 (warm_articles 错误)
//!
//! 注：spec §4 已删除 `refresh_news` 命令；refresh 走内部 facade，不通过 Tauri DTO 返回错误。

use crate::domain::shared::ErrorCode;
use serde::{Deserialize, Serialize};
use specta::Type;

/// `NewsErrorCode` 复用 shared `ErrorCode`；本模块仅使用其中一部分（spec §5 failure code 表）。
pub type NewsErrorCode = ErrorCode;

/// `warm_articles` 失败 error（spec §5）。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct WarmArticlesError {
    pub code: ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<WarmArticlesErrorField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum WarmArticlesErrorField {
    NewsIds,
    RecentLimit,
}
