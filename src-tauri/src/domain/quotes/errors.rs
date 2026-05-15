//! Quotes 模块统一错误类型。
//!
//! 区分 5 类——上层（agent tools / pipeline）可按类型差异化处理：
//! - `MissingToken`：用户没配 TuShare token——前端引导去 Settings
//! - `RateLimited` / `QuotaExceeded`：TuShare 限流 / 积分不足——重试 / 升档
//! - `Network`：网络层（reqwest 错误 / 超时）——重试有用
//! - `Decode`：接口响应 schema 异常——代码需要更新
//! - `Provider`：上游业务错误（code != 0）——按 msg 决定
//! - `NotFound`：请求的 code / 资源不存在
//! - `InvalidInput`：调用方参数错误

use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum QuotesError {
    #[error("未配置 TuShare token——请在 Settings → 数据源 里填入。注册：https://tushare.pro")]
    MissingToken,

    #[error("接口限流——稍后重试")]
    RateLimited,

    #[error("TuShare 积分不足——升档或减少调用频率")]
    QuotaExceeded,

    #[error("网络错误：{0}")]
    Network(String),

    #[error("响应解析失败：{0}")]
    Decode(String),

    #[error("{provider} 业务错误 [{code:?}]: {msg}")]
    Provider {
        provider: &'static str, // "tushare" | "em"
        code: Option<i64>,
        msg: String,
    },

    #[error("找不到资源：{0}")]
    NotFound(String),

    #[error("参数错误：{0}")]
    InvalidInput(String),
}

impl From<crate::domain::shared::IdError> for QuotesError {
    fn from(e: crate::domain::shared::IdError) -> Self {
        Self::InvalidInput(e.to_string())
    }
}

impl From<crate::domain::shared::MoneyError> for QuotesError {
    fn from(e: crate::domain::shared::MoneyError) -> Self {
        Self::Decode(e.to_string())
    }
}

impl From<crate::domain::shared::TimeError> for QuotesError {
    fn from(e: crate::domain::shared::TimeError) -> Self {
        Self::Decode(e.to_string())
    }
}

impl From<QuotesError> for String {
    fn from(e: QuotesError) -> Self {
        e.to_string()
    }
}
