//! ProviderChannel — 模型渠道配置。
//!
//! Spec: docs/design/agent-infra-module.md §2 `ProviderChannel`
//!
//! 抽象轴是 wire format，不是厂商。新增兼容厂商通常只新增 channel config。
//!
//! 不再有 `supportsTools` / `supportsServerSideTools` 字段：所有 chat-completable provider 都通过
//! Skill 文本协议（`<use_skill>`）提供工具能力，没有 provider 差异。

use serde::{Deserialize, Serialize};
use specta::Type;

/// serde default helper：`enabled` 默认 true。
fn default_true() -> bool {
    true
}

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
    /// 用户添加渠道时输入的「渠道名」，也是展示用 provider name；
    /// 前端按 `{model} ({provider})` 展示。无单独 provider-id 字段。
    pub provider: String,
    pub wire_format: WireFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// 鉴权 token；持久化但**只写不读**（list / get DTO 屏蔽明文，只回 apiKeySet）。
    #[serde(default)]
    pub api_key: String,
    pub model: String,
    /// Streaming 是主渠道硬要求；保留布尔字段对外明示。
    pub stream: bool,
    /// 渠道开关，默认 true。
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub supports_vision: bool,
    pub supports_thinking: bool,
    /// 模型生成上限，写入 provider request。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// 上下文窗口大小，驱动 soft / hard limit 计算。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<u32>,
    /// Anthropic extended-thinking budget。
    /// `None`（默认）= 不发 request-level `thinking` 配置（行为不变）。
    /// `Some(n)` 且 `supports_thinking = true` 时，Anthropic adapter 发
    /// top-level `thinking: {type:"enabled", budget_tokens:n}`，并校验 `n < max_tokens`。
    /// 参考 anthropic-messages.md §thinking：budget_tokens ≥ 1024 且 < max_tokens。
    ///
    /// **现状（runtime-only）**：wire 形状已就绪，但**尚未接入配置入口**——
    /// `agent_provider_channels` 没有对应列、`channels_repo` 读行时恒为 `None`、
    /// `agent_add_channel` 命令也不接收它。即默认关闭、当前无法经设置页 / DB 配置，
    /// 仅 Runtime 直接构造 channel 时可设。接入需加 DB 列 + DTO + migration（留待 Phase 3）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budget_tokens: Option<u32>,
}

impl ProviderChannel {
    /// 是否可以作为"主交易 Agent 渠道"——必须支持 streaming。
    /// Spec §2: 主渠道支持 streaming；不支持 streaming 的 provider 不能作为主渠道。
    pub fn is_primary_capable(&self) -> bool {
        self.stream
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
            api_key: String::new(),
            model: "claude-sonnet-4-5".into(),
            stream: true,
            enabled: true,
            supports_vision: true,
            supports_thinking: true,
            max_output_tokens: Some(8192),
            context_window_tokens: Some(200_000),
            thinking_budget_tokens: None,
        };
        let j = serde_json::to_value(&c).unwrap();
        assert_eq!(j["channelId"], "anthropic-main");
        assert_eq!(j["wireFormat"], "messages");
        assert_eq!(j["supportsVision"], true);
        assert_eq!(j["maxOutputTokens"], 8192);
        assert_eq!(j["contextWindowTokens"], 200_000);
    }

    #[test]
    fn primary_capable_requires_stream() {
        let mut c = ProviderChannel {
            channel_id: "x".into(),
            provider: "p".into(),
            wire_format: WireFormat::ChatCompletions,
            base_url: None,
            api_key: String::new(),
            model: "m".into(),
            stream: true,
            enabled: true,
            supports_vision: false,
            supports_thinking: false,
            max_output_tokens: None,
            context_window_tokens: None,
            thinking_budget_tokens: None,
        };
        assert!(c.is_primary_capable());
        c.stream = false;
        assert!(!c.is_primary_capable());
    }
}
