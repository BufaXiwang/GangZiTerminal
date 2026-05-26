//! 通用分页和 item 级 issue。
//!
//! Spec: docs/design/shared-types.md §7

use super::codes::{ErrorCode, WarningCode};
use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PageRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    pub limit: u32,
    pub offset: u32,
    pub has_more: bool,
}

/// item 级 issue。`code` 是前端和 Agent 判断行为的依据。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ItemIssue {
    pub code: ItemIssueCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// `ItemIssue.code` 可以是 `WarningCode` 或 `ErrorCode`（shared-types.md §7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(untagged)]
pub enum ItemIssueCode {
    Warning(WarningCode),
    Error(ErrorCode),
}
