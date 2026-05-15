//! 模型可用性探针——给 Settings 页"验证"按钮 + 保存前自检用。
//!
//! `verify_model` 发一发 1-token 的最小请求，确认 (provider, base_url, token,
//! model_id) 这四元组真能跑通。返回 `Ok(())` 或带 relay 原始错误文本的 Err。
//!
//! 自带 20s 超时——relay 可能会卡，UI 不能傻等。

use crate::agent::config::ProviderKind;
use crate::agent::provider::anthropic::ANTHROPIC_VERSION;
use crate::agent::provider::openai::common::build_http_client;
use crate::agent::provider::ProviderError;
use serde_json::json;
use std::time::Duration;

/// 发一发 1-token 最小请求，确认 (provider, base_url, token, model) 真能跑通。
///
/// 成本极低（输入 ~10 token + 输出 ≤1 token）。失败时 Err 里带 relay 的原始错误
/// 文本——例如 `model: claude-foo-bar` not_found / `temperature deprecated` 等。
pub async fn verify_model(
    provider: ProviderKind,
    base_url: &str,
    token: &str,
    model: &str,
) -> Result<(), ProviderError> {
    let http = build_http_client(Duration::from_secs(20))?;
    let stripped = base_url.trim_end_matches('/');
    match provider {
        ProviderKind::Anthropic => {
            let url = format!("{stripped}/v1/messages");
            let body = json!({
                "model": model,
                "max_tokens": 1,
                "stream": false,
                "messages": [{"role": "user", "content": "hi"}],
            });
            let resp = http
                .post(&url)
                .header("x-api-key", token)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|e| ProviderError::Transient(format!("verify 网络错误：{e}")))?;
            let status = resp.status();
            if status.is_success() {
                Ok(())
            } else {
                let body = resp.text().await.unwrap_or_default();
                Err(ProviderError::Request {
                    status: status.as_u16(),
                    body: format!("HTTP {} : {body}", status.as_u16()),
                })
            }
        }
        ProviderKind::OpenAIResponses => {
            let url = format!("{stripped}/v1/responses");
            let body = json!({
                "model": model,
                "max_output_tokens": 1,
                "stream": false,
                "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            });
            let resp = http
                .post(&url)
                .bearer_auth(token)
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|e| ProviderError::Transient(format!("verify 网络错误：{e}")))?;
            let status = resp.status();
            if status.is_success() {
                Ok(())
            } else {
                let body = resp.text().await.unwrap_or_default();
                Err(ProviderError::Request {
                    status: status.as_u16(),
                    body: format!("HTTP {} : {body}", status.as_u16()),
                })
            }
        }
        ProviderKind::OpenAIChatCompletions => {
            let url = format!("{stripped}/v1/chat/completions");
            let body = json!({
                "model": model,
                "max_tokens": 1,
                "stream": false,
                "messages": [{"role": "user", "content": "hi"}],
            });
            let resp = http
                .post(&url)
                .bearer_auth(token)
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|e| ProviderError::Transient(format!("verify 网络错误：{e}")))?;
            let status = resp.status();
            if status.is_success() {
                Ok(())
            } else {
                let body = resp.text().await.unwrap_or_default();
                Err(ProviderError::Request {
                    status: status.as_u16(),
                    body: format!("HTTP {} : {body}", status.as_u16()),
                })
            }
        }
    }
}

// 没单独的 unit test——verify_model 的语义是网络 round-trip，集成在 SettingsPage 流程
// 里 + cargo build 时确认编译通过即可。如果未来改翻译形态可以加 wire-format 测试。
