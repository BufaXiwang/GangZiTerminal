//! 两个 OpenAI provider 共用的小工具：HTTP client 构造 + reasoning effort 枚举。

use crate::infrastructure::agent::provider::ProviderError;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// gpt-5 / o3 系列的 reasoning effort。`None` 表示不发该字段（gpt-4 系列不识别）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
        }
    }
}

pub(crate) fn build_http_client(timeout: Duration) -> Result<Client, ProviderError> {
    Client::builder()
        .timeout(timeout)
        .pool_idle_timeout(Some(Duration::from_secs(90)))
        .build()
        .map_err(|err| ProviderError::Config(format!("构建 http client 失败：{err}")))
}

pub(crate) fn map_http_error(status: u16, body: String) -> ProviderError {
    match status {
        429 => ProviderError::RateLimited(body),
        500..=599 => ProviderError::Transient(format!("status={status} body={body}")),
        _ => ProviderError::Request { status, body },
    }
}

/// 把 base_url 去掉尾斜杠 + 校验是 http(s)。
pub(crate) fn normalize_base_url(raw: impl Into<String>) -> Result<String, ProviderError> {
    let url = raw.into().trim_end_matches('/').to_string();
    if !url.starts_with("http") {
        return Err(ProviderError::Config(format!(
            "openai base_url 不合法：{url}"
        )));
    }
    Ok(url)
}

pub(crate) fn require_token(token: &str) -> Result<(), ProviderError> {
    if token.trim().is_empty() {
        return Err(ProviderError::Config("openai token 为空".into()));
    }
    Ok(())
}
