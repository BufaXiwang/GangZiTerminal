//! 模型发现 + 快速预设。
//!
//! Spec: agent-infra-module.md §2 `ProviderChannel`（模型发现 + 确认 / 快速预设），§5 模型发现 API
//!
//! 用给定连接信息调对应 wireFormat 的 `/models` 接口，返回可用 model id 列表。
//! 发现失败（接口缺失 / 网络错）→ Err，调用方据此引导用户手填模型名。
//!
//! 这是普通 GET（非 streaming），属于渠道连通性能力；与「streaming `ProviderStream` 实现归
//! Runtime/Phase 3」不冲突。

use crate::domain::agent::WireFormat;
use serde::{Deserialize, Serialize};
use specta::Type;

/// 发现到的单个可用模型。
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoverError {
    #[error("network error: {0}")]
    Network(String),
    #[error("provider returned non-2xx status {status}: {body}")]
    Status { status: u16, body: String },
    #[error("failed to parse /models response: {0}")]
    Parse(String),
}

/// Anthropic version header 值（messages wire format）。
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// 构造 `/models` URL：`{base_url}/v1/models`（trim 尾部 '/'）。
pub fn build_models_url(base_url: &str) -> String {
    format!("{}/v1/models", base_url.trim_end_matches('/'))
}

/// 鉴权 header 列表 `(name, value)`：
/// - chat_completions | responses → `Authorization: Bearer {api_key}`
/// - messages → `x-api-key: {api_key}` + `anthropic-version: 2023-06-01`
pub fn auth_headers(wire_format: WireFormat, api_key: &str) -> Vec<(&'static str, String)> {
    match wire_format {
        WireFormat::ChatCompletions | WireFormat::Responses => {
            vec![("Authorization", format!("Bearer {api_key}"))]
        }
        WireFormat::Messages => vec![
            ("x-api-key", api_key.to_string()),
            ("anthropic-version", ANTHROPIC_VERSION.to_string()),
        ],
    }
}

/// `/models` 响应 wire shape：`{ "data": [ { "id": "...", "display_name"?: "..." } ] }`。
#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
}

/// 把原始 JSON body 解析成排序后的模型列表（纯函数，无网络，便于单测）。
fn parse_models(body: &str) -> Result<Vec<DiscoveredModel>, DiscoverError> {
    let resp: ModelsResponse =
        serde_json::from_str(body).map_err(|e| DiscoverError::Parse(e.to_string()))?;
    let mut models: Vec<DiscoveredModel> = resp
        .data
        .into_iter()
        .map(|e| DiscoveredModel {
            id: e.id,
            display_name: e.display_name,
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

/// 调对应 wireFormat 的 `/models` 接口发现可用模型。
///
/// Spec §5 模型发现 API。
pub async fn discover_models(
    wire_format: WireFormat,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<DiscoveredModel>, DiscoverError> {
    let url = build_models_url(base_url);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| DiscoverError::Network(e.to_string()))?;

    let mut req = client.get(&url);
    for (name, value) in auth_headers(wire_format, api_key) {
        req = req.header(name, value);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| DiscoverError::Network(e.to_string()))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| DiscoverError::Network(e.to_string()))?;

    if !status.is_success() {
        return Err(DiscoverError::Status {
            status: status.as_u16(),
            body: body.chars().take(500).collect(),
        });
    }

    parse_models(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_models_url_trims_trailing_slash() {
        assert_eq!(
            build_models_url("https://api.deepseek.com"),
            "https://api.deepseek.com/v1/models"
        );
        assert_eq!(
            build_models_url("https://api.deepseek.com/"),
            "https://api.deepseek.com/v1/models"
        );
        assert_eq!(
            build_models_url("https://coding.yiiyii.ai/api/"),
            "https://coding.yiiyii.ai/api/v1/models"
        );
    }

    #[test]
    fn auth_headers_bearer_for_openai_compatible() {
        let h = auth_headers(WireFormat::ChatCompletions, "k1");
        assert_eq!(h, vec![("Authorization", "Bearer k1".to_string())]);
        let h = auth_headers(WireFormat::Responses, "k2");
        assert_eq!(h, vec![("Authorization", "Bearer k2".to_string())]);
    }

    #[test]
    fn auth_headers_anthropic_for_messages() {
        let h = auth_headers(WireFormat::Messages, "k3");
        assert_eq!(
            h,
            vec![
                ("x-api-key", "k3".to_string()),
                ("anthropic-version", "2023-06-01".to_string()),
            ]
        );
    }

    #[test]
    fn parse_models_sorts_and_keeps_display_name() {
        let body = r#"{"object":"list","data":[
            {"id":"zeta","display_name":"Zeta"},
            {"id":"alpha"}
        ]}"#;
        let models = parse_models(body).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "alpha");
        assert_eq!(models[0].display_name, None);
        assert_eq!(models[1].id, "zeta");
        assert_eq!(models[1].display_name, Some("Zeta".to_string()));
    }

    #[test]
    fn parse_models_empty_data() {
        let models = parse_models(r#"{"object":"list","data":[]}"#).unwrap();
        assert!(models.is_empty());
    }

    #[test]
    fn parse_models_bad_json_errs() {
        assert!(parse_models("not json").is_err());
    }

    // ----- Live discovery (env-driven, no hardcoded secrets) -----
    //
    // Run with:
    //   TEST_DISCOVER_OAI_BASE=... TEST_DISCOVER_OAI_KEY=... \
    //   TEST_DISCOVER_ANT_BASE=... TEST_DISCOVER_ANT_KEY=... \
    //   TEST_DISCOVER_DS_KEY=... \
    //   cargo test discover_models_live -- --ignored --nocapture
    //
    // Each provider is skipped if its env vars are unset.
    #[tokio::test]
    #[ignore]
    async fn discover_models_live() {
        let mut ran = 0;

        if let (Ok(base), Ok(key)) = (
            std::env::var("TEST_DISCOVER_OAI_BASE"),
            std::env::var("TEST_DISCOVER_OAI_KEY"),
        ) {
            let models = discover_models(WireFormat::Responses, &base, &key)
                .await
                .expect("openai responses discovery failed");
            println!("[live] openai (responses) models: {}", models.len());
            assert!(!models.is_empty());
            ran += 1;
        } else {
            println!("[live] skip openai: TEST_DISCOVER_OAI_BASE/KEY unset");
        }

        if let (Ok(base), Ok(key)) = (
            std::env::var("TEST_DISCOVER_ANT_BASE"),
            std::env::var("TEST_DISCOVER_ANT_KEY"),
        ) {
            let models = discover_models(WireFormat::Messages, &base, &key)
                .await
                .expect("anthropic messages discovery failed");
            println!("[live] anthropic (messages) models: {}", models.len());
            assert!(!models.is_empty());
            ran += 1;
        } else {
            println!("[live] skip anthropic: TEST_DISCOVER_ANT_BASE/KEY unset");
        }

        if let Ok(key) = std::env::var("TEST_DISCOVER_DS_KEY") {
            let models =
                discover_models(WireFormat::ChatCompletions, "https://api.deepseek.com", &key)
                    .await
                    .expect("deepseek chat_completions discovery failed");
            println!("[live] deepseek (chat_completions) models: {}", models.len());
            assert!(!models.is_empty());
            ran += 1;
        } else {
            println!("[live] skip deepseek: TEST_DISCOVER_DS_KEY unset");
        }

        println!("[live] ran {ran} provider discovery checks");
    }
}
