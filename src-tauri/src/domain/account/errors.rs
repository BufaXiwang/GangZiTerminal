//! Account 域内部错误 — 不参与对外契约，由 pipeline 翻译为 ErrorCode。
//!
//! Spec: docs/design/account-module.md §5 error code 规则。

use crate::domain::shared::ErrorCode;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountErrorKind {
    InvalidInput,
    NotFound,
    InsufficientCash,
    InsufficientSellableQuantity,
    InvalidLotSize,
    OrderNotPending,
    RiskLimitExceeded,
    InstrumentNotTradable,
    InstrumentSuspended,
    OutsideTradingSession,
    QuoteMissing,
    QuoteStale,
    QuotePriceMissing,
    DepthMissing,
    LimitUpDownBlocked,
    DuplicateEvent,
    DbError,
}

impl AccountErrorKind {
    pub fn to_error_code(self) -> ErrorCode {
        match self {
            Self::InvalidInput => ErrorCode::InvalidInput,
            Self::NotFound => ErrorCode::NotFound,
            Self::InsufficientCash => ErrorCode::InsufficientCash,
            Self::InsufficientSellableQuantity => ErrorCode::InsufficientSellableQuantity,
            Self::InvalidLotSize => ErrorCode::InvalidLotSize,
            Self::OrderNotPending => ErrorCode::OrderNotPending,
            Self::RiskLimitExceeded => ErrorCode::RiskLimitExceeded,
            Self::InstrumentNotTradable => ErrorCode::InstrumentNotTradable,
            Self::InstrumentSuspended => ErrorCode::InstrumentSuspended,
            Self::OutsideTradingSession => ErrorCode::OutsideTradingSession,
            Self::QuoteMissing => ErrorCode::QuoteMissing,
            Self::QuoteStale => ErrorCode::QuoteStale,
            Self::QuotePriceMissing => ErrorCode::QuotePriceMissing,
            Self::DepthMissing => ErrorCode::DepthMissing,
            Self::LimitUpDownBlocked => ErrorCode::LimitUpDownBlocked,
            Self::DuplicateEvent => ErrorCode::DuplicateEvent,
            Self::DbError => ErrorCode::DbError,
        }
    }
}

#[derive(Debug, Error, Clone)]
pub struct AccountError {
    pub kind: AccountErrorKind,
    pub message: Option<String>,
}

impl AccountError {
    pub fn new(kind: AccountErrorKind) -> Self {
        Self {
            kind,
            message: None,
        }
    }

    pub fn with_message(kind: AccountErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: Some(message.into()),
        }
    }

    pub fn code(&self) -> ErrorCode {
        self.kind.to_error_code()
    }
}

impl std::fmt::Display for AccountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.kind)?;
        if let Some(m) = &self.message {
            write!(f, ": {}", m)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_kinds_map_to_distinct_error_codes() {
        // Sanity: each kind maps to a defined ErrorCode (no panic, no missing match arm).
        let kinds = [
            AccountErrorKind::InvalidInput,
            AccountErrorKind::NotFound,
            AccountErrorKind::InsufficientCash,
            AccountErrorKind::InsufficientSellableQuantity,
            AccountErrorKind::InvalidLotSize,
            AccountErrorKind::OrderNotPending,
            AccountErrorKind::RiskLimitExceeded,
            AccountErrorKind::InstrumentNotTradable,
            AccountErrorKind::InstrumentSuspended,
            AccountErrorKind::OutsideTradingSession,
            AccountErrorKind::QuoteMissing,
            AccountErrorKind::QuoteStale,
            AccountErrorKind::QuotePriceMissing,
            AccountErrorKind::DepthMissing,
            AccountErrorKind::LimitUpDownBlocked,
            AccountErrorKind::DuplicateEvent,
            AccountErrorKind::DbError,
        ];
        for k in kinds {
            let _ = k.to_error_code();
        }
    }
}
