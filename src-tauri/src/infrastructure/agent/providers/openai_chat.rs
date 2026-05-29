//! OpenAI Chat Completions channel adapter — 纯 chat（无 tool_use）。
//!
//! Spec: docs/design/agent-infra-module.md §2 ProviderChannel
//!       docs/design/references/agent/openai-chat-completions.md
//!
//! 兼容 deepseek / 阿里 / 豆包等 OpenAI-compatible 端点。

use super::{dereference_image, ProviderAdapter, WireMappingError};
use crate::domain::agent::context::ContextContent;
use crate::domain::agent::{
    AgentMessage, AgentMessageBlock, AgentMessageRole, AgentStopReason, ContextBundle,
    ProviderChannel, WireFormat,
};
use crate::infrastructure::agent::payload_store::PayloadStore;
use base64::Engine;
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

    fn message_to_chat(
        &self,
        msg: &AgentMessage,
        payload_store: Option<&PayloadStore>,
    ) -> Result<Option<Value>, WireMappingError> {
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
                        AgentMessageBlock::Image { mime_type, .. } => {
                            if !self.channel.supports_vision {
                                return Err(WireMappingError::VisionNotSupported);
                            }
                            let (bytes, ct) = dereference_image(b, payload_store)?;
                            let b64 =
                                base64::engine::general_purpose::STANDARD.encode(&bytes);
                            let media = if ct.is_empty() { mime_type.clone() } else { ct };
                            single_text = None;
                            parts.push(json!({
                                "type":"image_url",
                                "image_url": {"url": format!("data:{};base64,{}", media, b64)}
                            }));
                        }
                        AgentMessageBlock::Thinking { .. } => {
                            // Spec §2: user role 不允许 thinking; shouldn't reach here
                        }
                    }
                }
                let content: Value = if let (1, Some(t)) = (parts.len(), single_text) {
                    Value::String(t)
                } else {
                    Value::Array(parts)
                };
                Ok(Some(json!({"role":"user","content": content})))
            }
            AgentMessageRole::Assistant => {
                let mut text: Option<String> = None;
                for b in &msg.blocks {
                    match b {
                        AgentMessageBlock::Text { text: t } => {
                            text = Some(match text {
                                Some(prev) => prev + t,
                                None => t.clone(),
                            });
                        }
                        AgentMessageBlock::Thinking { .. } => {
                            // Spec §2: supports_thinking=false 时 adapter 静默丢弃；
                            // Chat Completions 普遍不支持 thinking 跨 turn 回写。
                        }
                        AgentMessageBlock::Image { .. } => {
                            // assistant 不允许 image; shouldn't reach here
                        }
                    }
                }
                let content_v = text.map(Value::String).unwrap_or(Value::Null);
                Ok(Some(json!({"role":"assistant","content": content_v})))
            }
        }
    }
}

impl ProviderAdapter for OpenAIChatAdapter {
    fn wire_format(&self) -> WireFormat {
        WireFormat::ChatCompletions
    }

    fn build_request_body(
        &self,
        msgs: &[AgentMessage],
        context: &ContextBundle,
        payload_store: Option<&PayloadStore>,
    ) -> Result<Value, WireMappingError> {
        let mut messages: Vec<Value> = Vec::new();
        let system_text = Self::extract_system(context);
        if !system_text.is_empty() {
            messages.push(json!({"role":"system","content": system_text}));
        }
        for m in msgs {
            m.validate_role_blocks()
                .map_err(|e| WireMappingError::InvalidMessage(format!("{:?}", e)))?;
            if let Some(v) = self.message_to_chat(m, payload_store)? {
                messages.push(v);
            }
        }
        let mut body = json!({
            "model": self.channel.model,
            "stream": self.channel.stream,
            "messages": messages,
        });
        if let Some(m) = self.channel.max_output_tokens {
            body["max_tokens"] = Value::from(m);
        }
        // Spec §3 / reference openai-chat-completions: stream 时请求 usage chunk。
        if self.channel.stream {
            body["stream_options"] = json!({"include_usage": true});
        }
        // Spec §2: 不传 tools 字段。
        Ok(body)
    }

    fn map_stop_reason(&self, raw: &str) -> AgentStopReason {
        match raw {
            "stop" => AgentStopReason::Completed,
            "length" => AgentStopReason::MaxTurns,
            "content_filter" => AgentStopReason::ProviderStop,
            "insufficient_system_resource" => AgentStopReason::Error,
            _ => AgentStopReason::Completed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::WireFormat;
    use chrono::Utc;

    fn ch(vision: bool) -> ProviderChannel {
        ProviderChannel {
            channel_id: "oai-chat".into(),
            provider: "deepseek".into(),
            wire_format: WireFormat::ChatCompletions,
            base_url: Some("https://api.deepseek.com".into()),
            api_key: String::new(),
            model: "deepseek-chat".into(),
            stream: true,
            enabled: true,
            supports_vision: vision,
            supports_thinking: false,
            max_output_tokens: Some(4096),
            context_window_tokens: Some(64_000),
        }
    }

    fn msg(role: AgentMessageRole, blocks: Vec<AgentMessageBlock>) -> AgentMessage {
        AgentMessage {
            message_id: "m1".into(),
            run_id: Some("r1".into()),
            role,
            blocks,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn user_message_becomes_string_content() {
        let ad = OpenAIChatAdapter::new(ch(false));
        let ctx = ContextBundle::new("r1");
        let msgs = vec![msg(
            AgentMessageRole::User,
            vec![AgentMessageBlock::Text { text: "hello".into() }],
        )];
        let body = ad.build_request_body(&msgs, &ctx, None).unwrap();
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
        assert_eq!(body["max_tokens"], 4096);
        assert!(body.get("tools").is_none());
        // Spec §3 / reference: stream 时带 stream_options.include_usage。
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn build_request_body_reflects_multi_message_conversation() {
        // Regression for the seed_messages bug.
        let ad = OpenAIChatAdapter::new(ch(false));
        let ctx = ContextBundle::new("r1");
        let msgs = vec![
            msg(
                AgentMessageRole::User,
                vec![AgentMessageBlock::Text { text: "查行情".into() }],
            ),
            msg(
                AgentMessageRole::Assistant,
                vec![AgentMessageBlock::Text {
                    text: r#"<use_skill name="x">{}</use_skill>"#.into(),
                }],
            ),
            msg(
                AgentMessageRole::User,
                vec![AgentMessageBlock::Text {
                    text: r#"<skill_result name="x" call_id="sc_1">{"ok":true}</skill_result>"#.into(),
                }],
            ),
        ];
        let body = ad.build_request_body(&msgs, &ctx, None).unwrap();
        let arr = body["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[2]["role"], "user");
        assert!(arr[2]["content"].as_str().unwrap().contains("<skill_result"));
    }

    #[test]
    fn assistant_skill_xml_passes_through_as_text() {
        let ad = OpenAIChatAdapter::new(ch(false));
        let ctx = ContextBundle::new("r1");
        let msgs = vec![msg(
            AgentMessageRole::Assistant,
            vec![AgentMessageBlock::Text {
                text: r#"thinking <use_skill name="x">{}</use_skill>"#.into(),
            }],
        )];
        let body = ad.build_request_body(&msgs, &ctx, None).unwrap();
        let m = &body["messages"][0];
        assert_eq!(m["role"], "assistant");
        let s = m["content"].as_str().unwrap();
        assert!(s.contains("<use_skill"));
    }

    #[test]
    fn vision_rejected_when_unsupported() {
        let ad = OpenAIChatAdapter::new(ch(false));
        let ctx = ContextBundle::new("r1");
        let msgs = vec![msg(
            AgentMessageRole::User,
            vec![AgentMessageBlock::Image {
                mime_type: "image/png".into(),
                data_ref: "payload://pl_x".into(),
            }],
        )];
        let err = ad.build_request_body(&msgs, &ctx, None).unwrap_err();
        assert!(matches!(err, WireMappingError::VisionNotSupported));
    }

    #[test]
    fn stop_reason_mapping() {
        let ad = OpenAIChatAdapter::new(ch(false));
        assert_eq!(ad.map_stop_reason("stop"), AgentStopReason::Completed);
        assert_eq!(ad.map_stop_reason("length"), AgentStopReason::MaxTurns);
        assert_eq!(
            ad.map_stop_reason("content_filter"),
            AgentStopReason::ProviderStop
        );
        assert_eq!(
            ad.map_stop_reason("insufficient_system_resource"),
            AgentStopReason::Error
        );
        assert_eq!(ad.map_stop_reason("anything"), AgentStopReason::Completed);
    }
}
