//! Provider channels — canonical request 与各 provider wire format 的互转。
//!
//! Spec: docs/design/agent-infra-module.md §2 `ProviderChannel`
//!       docs/design/references/agent/{anthropic-messages,openai-responses,openai-chat-completions}.md
//!
//! 模块拆分：
//! - `anthropic`           — Anthropic `/v1/messages`
//! - `openai_responses`    — OpenAI `/v1/responses`
//! - `openai_chat`         — OpenAI-compatible `/v1/chat/completions`
//!
//! 本目录只实现 **纯 chat request mapping**（text / image / thinking / usage / stop_reason）。
//! Skill 调用走 §2 定义的 `<use_skill>` 文本协议，**不**通过 provider 原生 tool_use / function_calling。
//! Provider request 中**不**传 `tools` 字段、**不**解析 `tool_use` / `function_call` block。

pub mod anthropic;
pub mod openai_chat;
pub mod openai_responses;

use crate::domain::agent::{
    AgentMessageBlock, AgentRunRequest, AgentStopReason, ContextBundle, WireFormat,
};
use crate::infrastructure::agent::payload_store::PayloadStore;

/// Provider request mapping 的 canonical 错误。
#[derive(Debug, thiserror::Error)]
pub enum WireMappingError {
    #[error("unsupported by wire format {0:?}: {1}")]
    Unsupported(WireFormat, String),
    #[error("invalid canonical message: {0}")]
    InvalidMessage(String),
    #[error("vision not supported by channel; image block disallowed")]
    VisionNotSupported,
    #[error("payload dereference failed: {0}")]
    PayloadDeref(String),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

/// 一个 provider channel adapter 必须实现的 mapping trait。
///
/// Spec: agent-infra-module.md §5 Infra Loop API
pub trait ProviderAdapter: Send + Sync {
    fn wire_format(&self) -> WireFormat;

    /// 把 canonical request + context 转成 provider 可接受的 JSON body。
    ///
    /// Spec §2 ProviderChannel:
    /// - `supports_vision = false` 时遇到 image block 返回 `VisionNotSupported`。
    /// - `supports_thinking = false` 时静默丢弃 thinking block。
    fn build_request_body(
        &self,
        request: &AgentRunRequest,
        context: &ContextBundle,
        payload_store: Option<&PayloadStore>,
    ) -> Result<serde_json::Value, WireMappingError>;

    fn map_stop_reason(&self, raw: &str) -> AgentStopReason;
}

/// Dereference `payload://pl_xxx` URI into bytes via PayloadStore.
/// Returns `(bytes, content_type)`. If `dataRef` is a non-payload URI (e.g. `file://`),
/// caller treats it as already-resolved external; for now we only support `payload://`.
pub fn dereference_image(
    block: &AgentMessageBlock,
    payload_store: Option<&PayloadStore>,
) -> Result<(Vec<u8>, String), WireMappingError> {
    let AgentMessageBlock::Image {
        mime_type,
        data_ref,
    } = block
    else {
        return Err(WireMappingError::InvalidMessage(
            "expected image block".into(),
        ));
    };
    if let Some(payload_id) = PayloadStore::parse_uri(data_ref) {
        let store = payload_store.ok_or_else(|| {
            WireMappingError::PayloadDeref(format!(
                "PayloadStore not provided; cannot resolve {}",
                data_ref
            ))
        })?;
        let entry = store
            .get(payload_id)
            .map_err(|e| WireMappingError::PayloadDeref(e.to_string()))?
            .ok_or_else(|| {
                WireMappingError::PayloadDeref(format!("payload {} not found", payload_id))
            })?;
        let bytes = entry
            .content_bytes
            .ok_or_else(|| WireMappingError::PayloadDeref(
                "payload entry has no bytes".into(),
            ))?;
        let ct = entry.content_type.unwrap_or_else(|| mime_type.clone());
        Ok((bytes, ct))
    } else {
        Err(WireMappingError::PayloadDeref(format!(
            "unsupported dataRef scheme: {}",
            data_ref
        )))
    }
}

#[cfg(test)]
mod factory_smoke {
    use super::*;
    use crate::domain::agent::{ProviderChannel, WireFormat};

    fn ch(wf: WireFormat) -> ProviderChannel {
        ProviderChannel {
            channel_id: "x".into(),
            provider: "p".into(),
            wire_format: wf,
            base_url: None,
            model: "m".into(),
            stream: true,
            supports_vision: false,
            supports_thinking: false,
            max_output_tokens: None,
            context_window_tokens: None,
        }
    }

    #[test]
    fn anthropic_adapter_reports_wire_format() {
        let a = anthropic::AnthropicAdapter::new(ch(WireFormat::Messages));
        assert_eq!(a.wire_format(), WireFormat::Messages);
    }

    #[test]
    fn responses_adapter_reports_wire_format() {
        let a = openai_responses::OpenAIResponsesAdapter::new(ch(WireFormat::Responses));
        assert_eq!(a.wire_format(), WireFormat::Responses);
    }

    #[test]
    fn chat_adapter_reports_wire_format() {
        let a = openai_chat::OpenAIChatAdapter::new(ch(WireFormat::ChatCompletions));
        assert_eq!(a.wire_format(), WireFormat::ChatCompletions);
    }
}
