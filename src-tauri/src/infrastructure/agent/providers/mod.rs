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
//! 本目录只实现 **request mapping** + **stream → AgentEvent** 抽象 trait；
//! 实际 HTTP / SSE 接线由 `loop_executor` 调用 trait。
//!
//! NOTE: Phase 1 把 wire format mapping 跑通到可测试程度；真实 provider 调用（reqwest + SSE
//! 解码）由后续迭代落地，不在本 Phase 阻塞 loop 测试。

pub mod anthropic;
pub mod openai_chat;
pub mod openai_responses;

use crate::domain::agent::{
    AgentRunRequest, AgentStopReason, ContextBundle, RunSummary, ToolSpec, WireFormat,
};

/// Provider request mapping 的 canonical 错误。
#[derive(Debug, thiserror::Error)]
pub enum WireMappingError {
    #[error("unsupported by wire format {0:?}: {1}")]
    Unsupported(WireFormat, String),
    #[error("invalid canonical message: {0}")]
    InvalidMessage(String),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

/// 一个 provider channel adapter 必须实现的 mapping trait。
///
/// Spec: agent-infra-module.md §5 Infra Loop API
///
/// Phase 1 阻塞实现的是 `build_request_body`；`run_stream` 暴露 trait 钩子但不在 Phase 1 接 HTTP。
pub trait ProviderAdapter: Send + Sync {
    /// Wire format 名。
    fn wire_format(&self) -> WireFormat;

    /// 把 canonical request + context + tool specs 转成 provider 可接受的 JSON body。
    fn build_request_body(
        &self,
        request: &AgentRunRequest,
        context: &ContextBundle,
        tools: &[ToolSpec],
    ) -> Result<serde_json::Value, WireMappingError>;

    /// 把 provider stop reason 文本归一化为 canonical `AgentStopReason`。
    fn map_stop_reason(&self, raw: &str) -> AgentStopReason;
}

/// 默认 RunSummary 构造（loop executor 完成后落盘）。
pub fn empty_run_summary(run_id: &str) -> RunSummary {
    RunSummary::empty(run_id)
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
            supports_tools: true,
            supports_vision: false,
            supports_thinking: false,
            supports_server_side_tools: None,
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
