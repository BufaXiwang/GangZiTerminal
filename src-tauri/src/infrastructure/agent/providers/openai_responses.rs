//! OpenAI Responses channel adapter。
//!
//! Spec: docs/design/references/agent/openai-responses.md

use super::{ProviderAdapter, WireMappingError};
use crate::domain::agent::context::ContextContent;
use crate::domain::agent::{
    AgentMessage, AgentMessageBlock, AgentMessageRole, AgentRunRequest, AgentStopReason,
    ContextBundle, ProviderChannel, ToolSpec, WireFormat,
};
use serde_json::{json, Value};

pub struct OpenAIResponsesAdapter {
    channel: ProviderChannel,
}

impl OpenAIResponsesAdapter {
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

    fn block_to_input_content(b: &AgentMessageBlock) -> Result<Option<Value>, WireMappingError> {
        Ok(match b {
            AgentMessageBlock::Text { text } => {
                Some(json!({"type":"input_text","text": text}))
            }
            AgentMessageBlock::Image { mime_type, data_ref } => Some(json!({
                "type":"input_image",
                "image_url": format!("data:{};base64,{}", mime_type, data_ref)
            })),
            // Responses 第一阶段丢弃 thinking
            AgentMessageBlock::Thinking { .. } => None,
            // tool_use / tool_result 走 top-level item，不进 content array
            AgentMessageBlock::ToolUse { .. } | AgentMessageBlock::ToolResult { .. } => None,
        })
    }

    fn message_to_input_items(msg: &AgentMessage) -> Result<Vec<Value>, WireMappingError> {
        let role = match msg.role {
            AgentMessageRole::System => return Ok(vec![]),
            AgentMessageRole::User => "user",
            AgentMessageRole::Assistant => "assistant",
            AgentMessageRole::Tool => "tool",
        };
        let mut items = Vec::new();
        let mut content = Vec::new();
        for b in &msg.blocks {
            match b {
                AgentMessageBlock::ToolUse {
                    tool_call_id,
                    name,
                    input_summary,
                } => {
                    items.push(json!({
                        "type":"function_call",
                        "call_id": tool_call_id,
                        "name": name,
                        "arguments": serde_json::to_string(input_summary)?
                    }));
                }
                AgentMessageBlock::ToolResult {
                    tool_call_id,
                    output_summary,
                    ..
                } => {
                    items.push(json!({
                        "type":"function_call_output",
                        "call_id": tool_call_id,
                        "output": serde_json::to_string(output_summary)?
                    }));
                }
                _ => {
                    if let Some(c) = Self::block_to_input_content(b)? {
                        content.push(c);
                    }
                }
            }
        }
        if !content.is_empty() {
            items.push(json!({
                "type":"message",
                "role": role,
                "content": content
            }));
        }
        Ok(items)
    }

    fn tool_spec_to_responses(t: &ToolSpec) -> Value {
        json!({
            "type":"function",
            "name": t.name,
            "description": t.description,
            "parameters": t.input_schema,
            "strict": false
        })
    }
}

impl ProviderAdapter for OpenAIResponsesAdapter {
    fn wire_format(&self) -> WireFormat {
        WireFormat::Responses
    }

    fn build_request_body(
        &self,
        request: &AgentRunRequest,
        context: &ContextBundle,
        tools: &[ToolSpec],
    ) -> Result<Value, WireMappingError> {
        let mut input: Vec<Value> = Vec::new();
        for m in &request.seed_messages {
            m.validate_role_blocks()
                .map_err(|e| WireMappingError::InvalidMessage(format!("{:?}", e)))?;
            input.extend(Self::message_to_input_items(m)?);
        }
        let mut body = json!({
            "model": self.channel.model,
            "stream": self.channel.stream,
            "input": input,
        });
        let system_text = Self::extract_system(context);
        if !system_text.is_empty() {
            body["instructions"] = Value::String(system_text);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.iter().map(Self::tool_spec_to_responses).collect());
        }
        // server-side web_search（spec request mapping 表）
        if request
            .allowed_server_side_tools
            .iter()
            .any(|s| s == "web_search")
        {
            let entry = json!({"type":"web_search"});
            if let Some(arr) = body["tools"].as_array_mut() {
                arr.push(entry);
            } else {
                body["tools"] = json!([{"type":"web_search"}]);
            }
        }
        Ok(body)
    }

    fn map_stop_reason(&self, raw: &str) -> AgentStopReason {
        match raw {
            "completed" => AgentStopReason::Completed,
            "max_output_tokens" | "length" => AgentStopReason::MaxTurns,
            "incomplete" => AgentStopReason::ProviderStop,
            _ => AgentStopReason::Completed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::{ContextBundle, ToolSideEffect, WireFormat};
    use chrono::Utc;

    fn ch() -> ProviderChannel {
        ProviderChannel {
            channel_id: "oai-responses".into(),
            provider: "openai".into(),
            wire_format: WireFormat::Responses,
            base_url: None,
            model: "gpt-5".into(),
            stream: true,
            supports_tools: true,
            supports_vision: true,
            supports_thinking: false,
            supports_server_side_tools: Some(vec!["web_search".into()]),
        }
    }

    #[test]
    fn maps_user_message_to_input_text() {
        let ad = OpenAIResponsesAdapter::new(ch());
        let ctx = ContextBundle::new("r1");
        let req = AgentRunRequest {
            run_id: "r1".into(),
            trigger: "u".into(),
            channel: ch(),
            max_turns: 1,
            allowed_server_side_tools: vec![],
            seed_messages: vec![AgentMessage {
                message_id: "m1".into(),
                run_id: Some("r1".into()),
                role: AgentMessageRole::User,
                blocks: vec![AgentMessageBlock::Text {
                    text: "hello".into(),
                }],
                created_at: Utc::now(),
            }],
        };
        let body = ad.build_request_body(&req, &ctx, &[]).unwrap();
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn maps_assistant_tool_use_to_function_call_item() {
        let ad = OpenAIResponsesAdapter::new(ch());
        let ctx = ContextBundle::new("r1");
        let req = AgentRunRequest {
            run_id: "r1".into(),
            trigger: "u".into(),
            channel: ch(),
            max_turns: 1,
            allowed_server_side_tools: vec![],
            seed_messages: vec![AgentMessage {
                message_id: "m1".into(),
                run_id: Some("r1".into()),
                role: AgentMessageRole::Assistant,
                blocks: vec![AgentMessageBlock::ToolUse {
                    tool_call_id: "tc1".into(),
                    name: "fetch_quote".into(),
                    input_summary: json!({"tsCode":"600519.SH"}),
                }],
                created_at: Utc::now(),
            }],
        };
        let body = ad.build_request_body(&req, &ctx, &[]).unwrap();
        let item = &body["input"][0];
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["call_id"], "tc1");
        assert_eq!(item["name"], "fetch_quote");
        let parsed: Value = serde_json::from_str(item["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(parsed["tsCode"], "600519.SH");
    }

    #[test]
    fn tool_result_role_maps_to_function_call_output_item() {
        let ad = OpenAIResponsesAdapter::new(ch());
        let ctx = ContextBundle::new("r1");
        let req = AgentRunRequest {
            run_id: "r1".into(),
            trigger: "u".into(),
            channel: ch(),
            max_turns: 1,
            allowed_server_side_tools: vec![],
            seed_messages: vec![AgentMessage {
                message_id: "m1".into(),
                run_id: Some("r1".into()),
                role: AgentMessageRole::Tool,
                blocks: vec![AgentMessageBlock::ToolResult {
                    tool_call_id: "tc1".into(),
                    output_summary: json!({"price":"100.5"}),
                    is_error: false,
                }],
                created_at: Utc::now(),
            }],
        };
        let body = ad.build_request_body(&req, &ctx, &[]).unwrap();
        let item = &body["input"][0];
        assert_eq!(item["type"], "function_call_output");
        assert_eq!(item["call_id"], "tc1");
    }

    #[test]
    fn tool_specs_emit_strict_false_function() {
        let ad = OpenAIResponsesAdapter::new(ch());
        let t = ToolSpec::new_local(
            "x",
            "x",
            json!({"type":"object"}),
            1000,
            ToolSideEffect::None,
        );
        let body = ad
            .build_request_body(
                &AgentRunRequest {
                    run_id: "r1".into(),
                    trigger: "u".into(),
                    channel: ch(),
                    max_turns: 1,
                    allowed_server_side_tools: vec!["web_search".into()],
                    seed_messages: vec![],
                },
                &ContextBundle::new("r1"),
                &[t],
            )
            .unwrap();
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["strict"], false);
        assert!(body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["type"] == "web_search"));
    }
}
