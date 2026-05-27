//! Warning / Error 封闭共享集合。
//!
//! Spec: docs/design/shared-types.md §5
//!
//! 规则：
//! - `WarningCode` 和 `ErrorCode` 是封闭共享集合；新增机器可读 code 必须先修改 shared-types.md，
//!   再被模块 spec 引用。
//! - 对外接口的机器可读错误必须用 code；人类可读 message 只能作为补充。

use serde::{Deserialize, Serialize};
use specta::Type;

use super::types::TsCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum WarningCode {
    QuoteMissing,
    QuoteStale,
    SnapshotExpired,
    QuotePriceMissing,
    DepthMissing,
    InstrumentMissing,
    ProviderPartialFailure,
    ArticleMissing,
    QfqMissing,
    UsingUnadjustedKline,
    DailyBasicMissing,
    EventsMissing,
    StrategyOmitted,
    MappingMissing,
    DataPartial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidInput,
    NotFound,
    ProviderUnavailable,
    RateLimited,
    DbError,
    ParseError,
    QuoteMissing,
    QuoteStale,
    QuotePriceMissing,
    DepthMissing,
    OutsideTradingSession,
    InstrumentNotTradable,
    InstrumentSuspended,
    LimitUpDownBlocked,
    InsufficientCash,
    InsufficientSellableQuantity,
    InvalidLotSize,
    OrderNotPending,
    RiskLimitExceeded,
    StrategyRequired,
    DuplicateEvent,
    VersionConflict,
    ArticleExtractFailed,
    ToolTimeout,
    ProviderContextTooLong,
}

/// Response 级 / item 级错误条目。
///
/// Spec: docs/design/quotes-module.md §4 `ResponseError`；shared-types.md §5。
/// 跨 BC 复用（News / Account / Quotes 都可以返回）。
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResponseError {
    pub code: ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts_code: Option<TsCode>,
}

impl ResponseError {
    pub fn new(code: ErrorCode) -> Self {
        Self {
            code,
            message: None,
            field: None,
            ts_code: None,
        }
    }
    pub fn with_message(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self {
            code,
            message: Some(msg.into()),
            field: None,
            ts_code: None,
        }
    }
    pub fn with_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }
    pub fn with_ts_code(mut self, ts_code: TsCode) -> Self {
        self.ts_code = Some(ts_code);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_code_serializes_snake_case() {
        let c = WarningCode::ProviderPartialFailure;
        assert_eq!(serde_json::to_string(&c).unwrap(), "\"provider_partial_failure\"");
    }

    #[test]
    fn error_code_serializes_snake_case() {
        let c = ErrorCode::InsufficientSellableQuantity;
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            "\"insufficient_sellable_quantity\""
        );
    }

    #[test]
    fn response_error_constructor_attaches_field() {
        let err = ResponseError::with_message(ErrorCode::InvalidInput, "bad")
            .with_field("tsCodes");
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert_eq!(err.field.as_deref(), Some("tsCodes"));
        assert_eq!(err.message.as_deref(), Some("bad"));
    }

    #[test]
    fn response_error_with_ts_code_round_trips() {
        let c = TsCode::parse("600519.SH").unwrap();
        let err = ResponseError::new(ErrorCode::NotFound).with_ts_code(c.clone());
        assert_eq!(err.ts_code, Some(c));
    }
}
