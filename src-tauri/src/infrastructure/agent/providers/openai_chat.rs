//! OpenAI Chat Completions channel adapter（OpenAI-compatible 兼容厂商）。
//!
//! Spec: docs/design/references/agent/openai-chat-completions.md

use super::{ProviderAdapter, WireMappingError};
use crate::domain::agent::context::ContextContent;
use crate::domain::agent::{
    AgentMessage, AgentMessageBlock, AgentMessageRole, AgentRunRequest, AgentStopReason,
    ContextBundle, ProviderChannel, ToolSpec, WireFormat,
};
use serde_json::{json, Value};

pub struct OpenAIChatAdapter {
    channel: ProviderChannel,
}

impl OpenAIChatAdapter {
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

    fn message_to_chat(msg: &AgentMessage) -> Result<Option<Value>, WireMappingError> {
        match msg.role {
            AgentMessageRole::System => {
                let text = msg
                    .blocks
                    .iter()
                    .filter_map(|b| {
                        if let AgentMessageBlock::Text { text } = b {
                            Some(text.clone())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(Some(json!({"role":"system","content": text})))
            }
            AgentMessageRole::User => {
                let mut parts: Vec<Value> = Vec::new();
                let mut single_text: Option<String> = None;
                for b in &msg.blocks {
                    match b {
                        AgentMessageBlock::Text { text } => {
                            single_text = Some(text.clone());
                            parts.push(json!({"type":"text","text": text}));
                        }
                        AgentMessageBlock::Image { mime_type, data_ref } => {
                            single_text = None;
                            parts.push(json!({
                                "type":"image_url",
                                "image_url": {"url": format!("data:{};base64,{}", mime_type, data_ref)}
                            }));
                        }
                        _ => {}
                    }
                }
                let content: Value =
                    if let (1, Some(t)) = (parts.len(), single_text) {
                        Value::String(t)
                    } else {
                        Value::Array(parts)
                    };
                Ok(Some(json!({"role":"user","content": content})))
            }
            AgentMessageRole::Assistant => {
                let mut text: Option<String> = None;
                let mut tool_calls: Vec<Value> = Vec::new();
                for b in &msg.blocks {
                    match b {
                        AgentMessageBlock::Text { text: t } => {
                            text = Some(match text {
                                Some(prev) => prev + t,
                                None => t.clone(),
                            });
                        }
                        AgentMessageBlock::Thinking { .. } => {
                            // Chat Completions 丢弃 thinking
                        }
                        AgentMessageBlock::ToolUse {
                            tool_call_id,
                            name,
                            input_summary,
                        } => {
                            tool_calls.push(json!({
                                "id": tool_call_id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": serde_json::to_string(input_summary)?
                                }
                            }));
                        }
                        _ => {}
                    }
                }
                let mut obj = serde_json::Map::new();
                obj.insert("role".into(), json!("assistant"));
                obj.insert(
                    "content".into(),
                    text.map(Value::String).unwrap_or(Value::Null),
                );
                if !tool_calls.is_empty() {
                    obj.insert("tool_calls".into(), Value::Array(tool_calls));
                }
                Ok(Some(Value::Object(obj)))
            }
            AgentMessageRole::Tool => {
                // tool result -> role=tool message keyed on tool_call_id
                let mut out: Vec<Value> = Vec::new();
                for b in &msg.blocks {
                    if let AgentMessageBlock::ToolResult {
                        tool_call_id,
                        output_summary,
                        ..
                    } = b
                    {
                        out.push(json!({
                            "role":"tool",
                            "tool_call_id": tool_call_id,
                            "content": serde_json::to_string(output_summary)?
                        }));
                    }
                }
                // 多 tool_result 会被外层展开；这里返回第一个，其余通过外部循环处理。
                // 这里我们把多结果合并为单结果意义不对；改为外层调用一次返回多 message。
                // 简化处理：将所有合并到一个 tool message 不符合 OpenAI 规范；
                // 因此外层循环（build_request_body）按需要为每个 tool_result 调用 message_to_chat
                // 已经按 message 粒度走，所以这里只允许 1 个 tool_result。
                if out.len() > 1 {
                    return Err(WireMappingError::InvalidMessage(
                        "OpenAI chat completions requires one tool_result per tool message".into(),
                    ));
                }
                Ok(out.into_iter().next())
            }
        }
    }

    fn tool_spec_to_chat(t: &ToolSpec) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema
            }
        })
    }
}

impl ProviderAdapter for OpenAIChatAdapter {
    fn wire_format(&self) -> WireFormat {
        WireFormat::ChatCompletions
    }

    fn build_request_body(
        &self,
        request: &AgentRunRequest,
        context: &ContextBundle,
        tools: &[ToolSpec],
    ) -> Result<Value, WireMappingError> {
        let mut messages: Vec<Value> = Vec::new();
        let system_text = Self::extract_system(context);
        if !system_text.is_empty() {
            messages.push(json!({"role":"system","content": system_text}));
        }
        for m in &request.seed_messages {
            m.validate_role_blocks()
                .map_err(|e| WireMappingError::InvalidMessage(format!("{:?}", e)))?;
            if let Some(v) = Self::message_to_chat(m)? {
                messages.push(v);
            }
        }
        let mut body = json!({
            "model": self.channel.model,
            "stream": self.channel.stream,
            "messages": messages,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.iter().map(Self::tool_spec_to_chat).collect());
        }
        // Chat Completions 不下发 server-side web_search（spec rules：不支持）。
        Ok(body)
    }

    fn map_stop_reason(&self, raw: &str) -> AgentStopReason {
        match raw {
            "stop" => AgentStopReason::Completed,
            "length" => AgentStopReason::MaxTurns,
            "tool_calls" => AgentStopReason::ProviderStop,
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
            channel_id: "oai-chat".into(),
            provider: "deepseek".into(),
            wire_format: WireFormat::ChatCompletions,
            base_url: Some("https://api.deepseek.com".into()),
            model: "deepseek-chat".into(),
            stream: true,
            supports_tools: true,
            supports_vision: false,
            supports_thinking: false,
            supports_server_side_tools: None,
        }
    }

    #[test]
    fn user_message_becomes_string_content() {
        let ad = OpenAIChatAdapter::new(ch());
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
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
    }

    #[test]
    fn assistant_with_tool_use_emits_tool_calls() {
        let ad = OpenAIChatAdapter::new(ch());
        let ctx = ContextBundle::new("r1");
        let req2 = AgentRunRequest {
            run_id: "r1".into(),
            trigger: "u".into(),
            channel: ch(),
            max_turns: 1,
            allowed_server_side_tools: vec![],
            seed_messages: vec![AgentMessage {
                message_id: "m1".into(),
                run_id: Some("r1".into()),
                role: AgentMessageRole::Assistant,
                blocks: vec![
                    AgentMessageBlock::Text {
                        text: "let me check".into(),
                    },
                    AgentMessageBlock::ToolUse {
                        tool_call_id: "tc1".into(),
                        name: "fetch_quote".into(),
                        input_summary: json!({"tsCode":"600519.SH"}),
                    },
                ],
                created_at: Utc::now(),
            }],
        };
        let body = ad.build_request_body(&req2, &ctx, &[]).unwrap();
        let m = &body["messages"][0];
        assert_eq!(m["role"], "assistant");
        assert_eq!(m["content"], "let me check");
        assert_eq!(m["tool_calls"][0]["id"], "tc1");
        assert_eq!(m["tool_calls"][0]["function"]["name"], "fetch_quote");
    }

    #[test]
    fn tool_result_becomes_role_tool_message() {
        let ad = OpenAIChatAdapter::new(ch());
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
        let m = &body["messages"][0];
        assert_eq!(m["role"], "tool");
        assert_eq!(m["tool_call_id"], "tc1");
    }

    #[test]
    fn does_not_emit_server_side_web_search() {
        let ad = OpenAIChatAdapter::new(ch());
        let ctx = ContextBundle::new("r1");
        let req = AgentRunRequest {
            run_id: "r1".into(),
            trigger: "u".into(),
            channel: ch(),
            max_turns: 1,
            allowed_server_side_tools: vec!["web_search".into()],
            seed_messages: vec![],
        };
        let body = ad
            .build_request_body(
                &req,
                &ctx,
                &[ToolSpec::new_local(
                    "x",
                    "x",
                    json!({}),
                    1000,
                    ToolSideEffect::None,
                )],
            )
            .unwrap();
        // tools must contain only the local function; no web_search injected
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "x");
    }
}
