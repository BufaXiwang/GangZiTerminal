//! News 域错误。
//!
//! infrastructure 层（fetchers / article extractor）把外部错误（reqwest / serde_json /
//! rss）map 成 `NewsError` 的某个 variant；pipeline 层只看抽象类型决定降级策略。
//!
//! 跨模块事件（spec `NewsFailure.code`）要求机器可读 ErrorCode，pipeline 层在
//! emit 失败时调 [`NewsError::to_error_code`] 收敛到 shared 集合。

use crate::domain::shared::ErrorCode;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NewsError {
    /// HTTP / 网络层失败（连接错误、超时、非 2xx 状态）。
    #[error("网络错误：{0}")]
    Network(String),
    /// 远端响应解析失败（JSON / RSS / HTML schema 不对）。
    #[error("解析失败：{0}")]
    Decode(String),
    /// 配置缺失（base_url 空、URL 无效等）。
    #[error("配置错误：{0}")]
    Config(String),
}

impl NewsError {
    /// 把内部错误 variant 收敛到 spec `ErrorCode` 闭集合（spec shared-types.md §5）。
    pub fn to_error_code(&self) -> ErrorCode {
        match self {
            NewsError::Network(msg) => {
                // HTTP 429 / 5xx 走 rate_limited，其余走 provider_unavailable
                let lower = msg.to_ascii_lowercase();
                if lower.contains("429") || lower.contains("rate limit") {
                    ErrorCode::RateLimited
                } else {
                    ErrorCode::ProviderUnavailable
                }
            }
            NewsError::Decode(_) => ErrorCode::ParseError,
            NewsError::Config(_) => ErrorCode::InvalidInput,
        }
    }

    /// 是否值得重试。网络瞬时错可重试，parse / config 错不可。
    pub fn is_retryable(&self) -> bool {
        matches!(self, NewsError::Network(_))
    }
}
