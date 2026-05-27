//! Anthropic Messages channel adapter。
//!
//! Spec: docs/design/references/agent/anthropic-messages.md
//!
//! 实现 Phase 1：request body 映射 + stop_reason 归一化。
//! HTTP / SSE 接线由后续迭代落地。

use super::{ProviderAdapter, WireMappingError};
use crate::domain::agent::{
    AgentMessage, AgentMessageBlock, AgentMessageRole, AgentRunRequest, AgentStopReason,
    ContextBundle, ProviderChannel, ToolSpec, WireFormat,
};
use crate::domain::agent::context::ContextContent;
use serde_json::{json, Value};

pub struct AnthropicAdapter {
    channel: ProviderChannel,
}

impl AnthropicAdapter {
    pub fn new(channel: ProviderChannel) -> Self {
        Self { channel }
    }

    fn extract_system(context: &ContextBundle) -> String {
        context
            .system_parts
            .iter()
            .filter_map(|p| match &p.content {
                ContextContent::Text(s) => Some(s.clone()),
                ContextContent::Json(v) => Some(v.to_string()),
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn block_to_anthropic(b: &AgentMessageBlock) -> Result<Value, WireMappingError> {
        Ok(match b {
            AgentMessageBlock::Text { text } => json!({ "type": "text", "text": text }),
            AgentMessageBlock::Image { mime_type, data_ref } => json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": mime_type,
                    "data": data_ref
                }
            }),
            AgentMessageBlock::Thinking { text, provider: _ } => {
                json!({ "type": "thinking", "thinking": text })
            }
            AgentMessageBlock::ToolUse {
                tool_call_id,
                name,
                input_summary,
            } => json!({
                "type": "tool_use",
                "id": tool_call_id,
                "name": name,
                "input": input_summary
            }),
            AgentMessageBlock::ToolResult {
                tool_call_id,
                output_summary,
                is_error,
            } => json!({
                "type": "tool_result",
                "tool_use_id": tool_call_id,
                "content": output_summary,
                "is_error": is_error
            }),
        })
    }

    fn message_to_anthropic(msg: &AgentMessage) -> Result<Option<Value>, WireMappingError> {
        let role = match msg.role {
            // system 走 top-level `system`
            AgentMessageRole::System => return Ok(None),
            AgentMessageRole::User => "user",
            AgentMessageRole::Assistant => "assistant",
            AgentMessageRole::Tool => "user", // Anthropic 把 tool_result 包在 user message 内
        };
        let content = msg
            .blocks
            .iter()
            .map(Self::block_to_anthropic)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(json!({
            "role": role,
            "content": content
        })))
    }

    fn tool_spec_to_anthropic(t: &ToolSpec) -> Value {
        json!({
            "name": t.name,
            "description": t.description,
            "input_schema": t.input_schema
        })
    }
}

impl ProviderAdapter for AnthropicAdapter {
    fn wire_format(&self) -> WireFormat {
        WireFormat::Messages
    }

    fn build_request_body(
        &self,
        request: &AgentRunRequest,
        context: &ContextBundle,
        tools: &[ToolSpec],
    ) -> Result<Value, WireMappingError> {
        let system_text = Self::extract_system(context);
        let mut messages = Vec::with_capacity(request.seed_messages.len());
        for m in &request.seed_messages {
            m.validate_role_blocks()
                .map_err(|e| WireMappingError::InvalidMessage(format!("{:?}", e)))?;
            if let Some(v) = Self::message_to_anthropic(m)? {
                messages.push(v);
            }
        }
        let mut body = json!({
            "model": self.channel.model,
            "stream": self.channel.stream,
            "max_tokens": 8192,
            "messages": messages,
        });
        if !system_text.is_empty() {
            body["system"] = Value::String(system_text);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.iter().map(Self::tool_spec_to_anthropic).collect());
        }
        // server-side web_search（spec request mapping 表）
        let server = &request.allowed_server_side_tools;
        if server.iter().any(|s| s == "web_search") {
            if let Some(arr) = body["tools"].as_array_mut() {
                arr.push(json!({ "type": "web_search_20250305", "name": "web_search" }));
            } else {
                body["tools"] =
                    json!([{ "type": "web_search_20250305", "name": "web_search" }]);
            }
        }
        Ok(body)
    }

    fn map_stop_reason(&self, raw: &str) -> AgentStopReason {
        match raw {
            "end_turn" => AgentStopReason::Completed,
            "max_tokens" => AgentStopReason::MaxTurns,
            "tool_use" => AgentStopReason::ProviderStop,
            "stop_sequence" => AgentStopReason::ProviderStop,
            _ => AgentStopReason::Completed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::{
        ContextBundle, ToolSideEffect, ToolSpec, WireFormat,
    };
    use chrono::Utc;

    fn make_channel() -> ProviderChannel {
        ProviderChannel {
            channel_id: "anthropic".into(),
            provider: "anthropic".into(),
            wire_format: WireFormat::Messages,
            base_url: None,
            model: "claude-sonnet-4-5".into(),
            stream: true,
            supports_tools: true,
            supports_vision: true,
            supports_thinking: false,
            supports_server_side_tools: Some(vec!["web_search".into()]),
        }
    }

    #[test]
    fn build_request_body_fixture_messages() {
        let ad = AnthropicAdapter::new(make_channel());
        let mut ctx = ContextBundle::new("r1");
        ctx.system_parts.push(crate::domain::agent::ContextPart {
            kind: crate::domain::agent::ContextPartKind::System,
            content: ContextContent::Text("You are Gangzi.".into()),
            freshness: None,
            token_estimate: None,
            droppable: false,
        });
        let req = AgentRunRequest {
            run_id: "r1".into(),
            trigger: "user".into(),
            channel: make_channel(),
            max_turns: 8,
            allowed_server_side_tools: vec![],
            seed_messages: vec![AgentMessage {
                message_id: "m1".into(),
                run_id: Some("r1".into()),
                role: AgentMessageRole::User,
                blocks: vec![AgentMessageBlock::Text {
                    text: "trade 600519.SH plan".into(),
                }],
                created_at: Utc::now(),
            }],
        };
        let tool = ToolSpec::new_local(
            "fetch_quote",
            "read latest quote",
            json!({"type":"object","properties":{"tsCode":{"type":"string"}},"required":["tsCode"]}),
            5000,
            ToolSideEffect::None,
        );
        let body = ad.build_request_body(&req, &ctx, &[tool]).unwrap();
        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["system"], "You are Gangzi.");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert_eq!(body["tools"][0]["name"], "fetch_quote");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn build_request_body_appends_server_side_web_search_when_allowed() {
        let ad = AnthropicAdapter::new(make_channel());
        let ctx = ContextBundle::new("r1");
        let req = AgentRunRequest {
            run_id: "r1".into(),
            trigger: "u".into(),
            channel: make_channel(),
            max_turns: 2,
            allowed_server_side_tools: vec!["web_search".into()],
            seed_messages: vec![],
        };
        let body = ad.build_request_body(&req, &ctx, &[]).unwrap();
        let tools = body["tools"].as_array().unwrap();
        assert!(tools
            .iter()
            .any(|t| t["type"] == "web_search_20250305" && t["name"] == "web_search"));
    }

    #[test]
    fn tool_result_block_maps_to_anthropic_user_role() {
        let ad = AnthropicAdapter::new(make_channel());
        let ctx = ContextBundle::new("r1");
        let req = AgentRunRequest {
            run_id: "r1".into(),
            trigger: "x".into(),
            channel: make_channel(),
            max_turns: 1,
            allowed_server_side_tools: vec![],
            seed_messages: vec![AgentMessage {
                message_id: "m1".into(),
                run_id: Some("r1".into()),
                role: AgentMessageRole::Tool,
                blocks: vec![AgentMessageBlock::ToolResult {
                    tool_call_id: "tc1".into(),
                    output_summary: json!({"ok":1}),
                    is_error: false,
                }],
                created_at: Utc::now(),
            }],
        };
        let body = ad.build_request_body(&req, &ctx, &[]).unwrap();
        // tool result is wrapped inside a `user` role message with tool_result block
        let msg = &body["messages"][0];
        assert_eq!(msg["role"], "user");
        assert_eq!(msg["content"][0]["type"], "tool_result");
        assert_eq!(msg["content"][0]["tool_use_id"], "tc1");
    }

    #[test]
    fn map_stop_reason_known_codes() {
        let ad = AnthropicAdapter::new(make_channel());
        assert_eq!(ad.map_stop_reason("end_turn"), AgentStopReason::Completed);
        assert_eq!(ad.map_stop_reason("max_tokens"), AgentStopReason::MaxTurns);
        assert_eq!(
            ad.map_stop_reason("tool_use"),
            AgentStopReason::ProviderStop
        );
    }
}
