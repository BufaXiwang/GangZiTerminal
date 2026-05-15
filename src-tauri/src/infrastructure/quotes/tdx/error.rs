//! TDX 模块错误类型——独立于 `crate::domain::quotes::QuotesError`，上层包装时转换。

use std::io;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("unexpected response length: expected {expected}, got {got}")]
    BadLength { expected: usize, got: usize },

    #[error("zlib decompression failed: {0}")]
    Decompress(String),

    #[error("not connected")]
    NotConnected,

    #[error("file not found: {0}")]
    FileNotFound(String),

    #[error("invalid security exchange in filename: {0}")]
    UnknownExchange(String),

    #[error("invalid symbol: {0}")]
    InvalidSymbol(String),

    #[error("invalid record (len {0} not multiple of {1})")]
    InvalidRecordSize(usize, usize),
}
