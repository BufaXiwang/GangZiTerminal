//! ProviderChannel — 模型渠道配置。
//!
//! Spec: docs/design/agent-infra-module.md §2 `ProviderChannel`
//!
//! 抽象轴是 wire format，不是厂商。新增兼容厂商通常只新增 channel config。

use serde::{Deserialize, Serialize};
use specta::Type;

/// Wire format 标识 — channel adapter 选择哪个 provider 实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum WireFormat {
    /// Anthropic `/v1/messages`
    Messages,
    /// OpenAI `/v1/responses`
    Responses,
    /// OpenAI-compatible `/v1/chat/completions`
    ChatCompletions,
}

/// 模型渠道配置。
///
/// Spec: agent-infra-module.md §2 `ProviderChannel`
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderChannel {
    pub channel_id: String,
    pub provider: String,
    pub wire_format: WireFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    pub model: String,
    /// Streaming 是主渠道硬要求；保留布尔字段对外明示。
    pub stream: bool,
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub supports_thinking: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_server_side_tools: Option<Vec<String>>,
}

impl ProviderChannel {
    /// 是否可以作为"主交易 Agent 渠道"——必须支持 streaming + local tools。
    /// Spec §2 `ProviderChannel` rules + agent-infra-module.md §6 验收。
    pub fn is_primary_capable(&self) -> bool {
        self.stream && self.supports_tools
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_channel_serde_camel_case() {
        let c = ProviderChannel {
            channel_id: "anthropic-main".into(),
            provider: "anthropic".into(),
            wire_format: WireFormat::Messages,
            base_url: None,
            model: "claude-sonnet-4-5".into(),
            stream: true,
            supports_tools: true,
            supports_vision: true,
            supports_thinking: true,
            supports_server_side_tools: Some(vec!["web_search".into()]),
        };
        let j = serde_json::to_value(&c).unwrap();
        assert_eq!(j["channelId"], "anthropic-main");
        assert_eq!(j["wireFormat"], "messages");
        assert_eq!(j["supportsTools"], true);
        assert_eq!(j["supportsServerSideTools"][0], "web_search");
    }

    #[test]
    fn primary_capable_requires_stream_and_tools() {
        let mut c = ProviderChannel {
            channel_id: "x".into(),
            provider: "p".into(),
            wire_format: WireFormat::ChatCompletions,
            base_url: None,
            model: "m".into(),
            stream: true,
            supports_tools: true,
            supports_vision: false,
            supports_thinking: false,
            supports_server_side_tools: None,
        };
        assert!(c.is_primary_capable());
        c.supports_tools = false;
        assert!(!c.is_primary_capable());
    }
}
