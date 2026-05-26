//! News 模块错误类型 — DTO 级 error。
//!
//! Spec: docs/design/news-module.md §4 (refresh_news / warm_articles 错误)

use crate::domain::shared::ErrorCode;
use serde::{Deserialize, Serialize};
use specta::Type;

/// `NewsErrorCode` 复用 shared `ErrorCode`；本模块仅使用其中一部分（spec §4 失败码表 + §5）。
pub type NewsErrorCode = ErrorCode;

/// `refresh_news` 失败 error（spec §4）。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RefreshNewsError {
    pub code: ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<RefreshNewsErrorField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum RefreshNewsErrorField {
    Sources,
    Force,
}

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
