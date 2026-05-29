//! 渠道快速预设 — 内置已知厂商官方端点。
//!
//! Spec: agent-infra-module.md §2 `ProviderChannel`（渠道添加方式 1：快速预设）
//!
//! 快速预设预置 `provider` 名 + `wireFormat` + `baseUrl`，用户只需填 apiKey。
//! 注意：OpenAI 使用 **responses** wire format。

use crate::domain::agent::WireFormat;

/// 一条快速预设。
pub struct ChannelPreset {
    pub key: &'static str,
    pub provider: &'static str,
    pub wire_format: WireFormat,
    pub base_url: &'static str,
}

/// 内置快速预设列表。
pub fn channel_presets() -> Vec<ChannelPreset> {
    vec![
        ChannelPreset {
            key: "deepseek",
            provider: "DeepSeek",
            wire_format: WireFormat::ChatCompletions,
            base_url: "https://api.deepseek.com",
        },
        ChannelPreset {
            key: "openai",
            provider: "OpenAI",
            wire_format: WireFormat::Responses,
            base_url: "https://api.openai.com",
        },
        ChannelPreset {
            key: "anthropic",
            provider: "Anthropic",
            wire_format: WireFormat::Messages,
            base_url: "https://api.anthropic.com",
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_cover_three_providers() {
        let p = channel_presets();
        assert_eq!(p.len(), 3);
        let keys: Vec<&str> = p.iter().map(|x| x.key).collect();
        assert!(keys.contains(&"deepseek"));
        assert!(keys.contains(&"openai"));
        assert!(keys.contains(&"anthropic"));
    }

    #[test]
    fn openai_preset_uses_responses_wire_format() {
        let p = channel_presets();
        let openai = p.iter().find(|x| x.key == "openai").unwrap();
        assert_eq!(openai.wire_format, WireFormat::Responses);
        assert_eq!(openai.provider, "OpenAI");
        assert_eq!(openai.base_url, "https://api.openai.com");
    }

    #[test]
    fn deepseek_preset_chat_completions() {
        let p = channel_presets();
        let ds = p.iter().find(|x| x.key == "deepseek").unwrap();
        assert_eq!(ds.wire_format, WireFormat::ChatCompletions);
    }

    #[test]
    fn anthropic_preset_messages() {
        let p = channel_presets();
        let ant = p.iter().find(|x| x.key == "anthropic").unwrap();
        assert_eq!(ant.wire_format, WireFormat::Messages);
    }
}
