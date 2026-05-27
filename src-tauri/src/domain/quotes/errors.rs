//! Quotes domain 内部错误类型。
//!
//! Spec: docs/design/quotes-module.md §4

use crate::domain::shared::ErrorCode;
use thiserror::Error;

/// Snapshot facade / pipeline 内部错误。
///
/// Spec: quotes-module.md §4 内部 Rust API（Account 等会读 facade）。
#[derive(Debug, Error, Clone)]
pub struct QuoteFacadeError {
    pub kind: QuoteFacadeErrorKind,
    pub message: Option<String>,
}

impl QuoteFacadeError {
    pub fn new(kind: QuoteFacadeErrorKind) -> Self {
        Self {
            kind,
            message: None,
        }
    }
    pub fn with_message(kind: QuoteFacadeErrorKind, msg: impl Into<String>) -> Self {
        Self {
            kind,
            message: Some(msg.into()),
        }
    }
    pub fn code(&self) -> ErrorCode {
        match self.kind {
            QuoteFacadeErrorKind::InvalidInput => ErrorCode::InvalidInput,
            QuoteFacadeErrorKind::NotFound => ErrorCode::NotFound,
            QuoteFacadeErrorKind::QuoteMissing => ErrorCode::QuoteMissing,
            QuoteFacadeErrorKind::QuoteStale => ErrorCode::QuoteStale,
            QuoteFacadeErrorKind::QuotePriceMissing => ErrorCode::QuotePriceMissing,
            QuoteFacadeErrorKind::DepthMissing => ErrorCode::DepthMissing,
            QuoteFacadeErrorKind::DbError => ErrorCode::DbError,
        }
    }
}

impl std::fmt::Display for QuoteFacadeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.kind)?;
        if let Some(m) = &self.message {
            write!(f, ": {}", m)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteFacadeErrorKind {
    InvalidInput,
    NotFound,
    QuoteMissing,
    QuoteStale,
    QuotePriceMissing,
    DepthMissing,
    DbError,
}
