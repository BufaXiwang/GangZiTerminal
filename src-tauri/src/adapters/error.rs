//! Tauri command 错误 DTO + domain ErrorCode 映射。
//!
//! Spec: docs/design/shared-types.md §5
//!
//! 规则：
//! - 对外接口的机器可读错误必须用 ErrorCode；message 仅作补充。
//! - 写接口失败必须返回单一主 reason code，可附带 details。

use crate::domain::shared::{ErrorCode, JsonValue};
use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CommandError {
    pub code: ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonValue>,
}

impl CommandError {
    pub fn new(code: ErrorCode) -> Self {
        Self {
            code,
            message: None,
            details: None,
        }
    }

    pub fn with_message(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: Some(message.into()),
            details: None,
        }
    }
}

impl From<ErrorCode> for CommandError {
    fn from(code: ErrorCode) -> Self {
        CommandError::new(code)
    }
}
