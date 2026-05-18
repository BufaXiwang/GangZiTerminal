//! News 域错误。
//!
//! infrastructure 层（fetchers / article extractor）把外部错误（reqwest / serde_json /
//! rss）map 成 `NewsError` 的某个 variant；pipeline 层只看抽象类型决定降级策略。

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
    /// 响应过大（防 SSRF / 防爆内存）。
    #[error("响应过大：{0}")]
    TooLarge(String),
    /// 状态流转不符合 NewsStatus 状态机。
    #[error("状态错误：{0}")]
    InvalidState(String),
}
