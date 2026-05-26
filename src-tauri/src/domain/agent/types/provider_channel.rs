//! `ProviderChannel` —— spec `agent-infra-module.md §2`。
//!
//! 把"调用一个 LLM 模型"所需的全部稳定能力声明拢成一个 channel 描述符。
//! Pipeline 注册多个 channel；Runtime / 前端 / observer 可按 channel id 查询
//! 能力布尔字段（supportsTools / supportsVision / supportsThinking /
//! supportsServerSideTools）。
//!
//! 验证规则（spec §3）：主渠道（primary）必须 `supports_tools = true`，否则
//! 不能用作 tool-using agent loop 的 driver。

use serde::{Deserialize, Serialize};

use super::request::ProviderKind;

/// spec §2 `ProviderChannel`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderChannel {
    pub channel_id: String,
    pub provider: ProviderKind,
    pub wire_format: WireFormat,
    pub base_url: String,
    pub model: String,
    #[serde(default = "true_default")]
    pub stream: bool,
    #[serde(default)]
    pub supports_tools: bool,
    #[serde(default)]
    pub supports_vision: bool,
    #[serde(default)]
    pub supports_thinking: bool,
    #[serde(default)]
    pub supports_server_side_tools: bool,
}

fn true_default() -> bool {
    true
}

/// spec §2 wire format 闭集合。当前与 `ProviderKind` 同构，但单独命名让能力
/// 声明语义独立于 provider 派生。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireFormat {
    Anthropic,
    OpenaiResponses,
    OpenaiChatCompletions,
}

impl WireFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            WireFormat::Anthropic => "anthropic",
            WireFormat::OpenaiResponses => "openai_responses",
            WireFormat::OpenaiChatCompletions => "openai_chat_completions",
        }
    }

    pub fn from_provider(p: ProviderKind) -> Self {
        match p {
            ProviderKind::Anthropic => WireFormat::Anthropic,
            ProviderKind::OpenAIResponses => WireFormat::OpenaiResponses,
            ProviderKind::OpenAIChatCompletions => WireFormat::OpenaiChatCompletions,
        }
    }
}

/// spec §3「主渠道必须 supportsTools = true」。
#[derive(Debug, Clone)]
pub struct PrimaryChannelMustSupportTools {
    pub channel_id: String,
}

impl std::fmt::Display for PrimaryChannelMustSupportTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "primary provider channel `{}` 缺 supports_tools = true（spec agent-infra-module.md §3）",
            self.channel_id
        )
    }
}

impl std::error::Error for PrimaryChannelMustSupportTools {}

impl ProviderChannel {
    pub fn assert_primary_supports_tools(&self) -> Result<(), PrimaryChannelMustSupportTools> {
        if self.supports_tools {
            Ok(())
        } else {
            Err(PrimaryChannelMustSupportTools {
                channel_id: self.channel_id.clone(),
            })
        }
    }
}
