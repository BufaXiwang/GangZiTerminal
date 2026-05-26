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
}
