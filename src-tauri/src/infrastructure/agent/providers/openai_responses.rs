//! OpenAI Responses channel adapter — 纯 chat（无 tool_use）。
//!
//! Spec: docs/design/agent-infra-module.md §2 ProviderChannel
//!       docs/design/references/agent/openai-responses.md

use super::{dereference_image, ProviderAdapter, WireMappingError};
use crate::domain::agent::context::ContextContent;
use crate::domain::agent::{
    AgentMessage, AgentMessageBlock, AgentMessageRole, AgentStopReason, ContextBundle,
    ProviderChannel, WireFormat,
};
use crate::infrastructure::agent::payload_store::PayloadStore;
use base64::Engine;
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
            .map(|p| match &p.content {
                ContextContent::Text(s) => s.clone(),
                ContextContent::Json(v) => v.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn block_to_input_content(
        &self,
        b: &AgentMessageBlock,
        role: AgentMessageRole,
        payload_store: Option<&PayloadStore>,
    ) -> Result<Option<Value>, WireMappingError> {
        // Responses API: assistant-role message items carry `output_text`; user/system carry
        // `input_text`. Emitting `input_text` for an assistant turn makes the multi-turn `input`
        // array malformed and the upstream rejects the whole request (HTTP 502 upstream_error).
        let text_type = match role {
            AgentMessageRole::Assistant => "output_text",
            _ => "input_text",
        };
        Ok(match b {
            AgentMessageBlock::Text { text } => {
                Some(json!({"type": text_type, "text": text}))
            }
            AgentMessageBlock::Image { mime_type, .. } => {
                if !self.channel.supports_vision {
                    return Err(WireMappingError::VisionNotSupported);
                }
                let (bytes, ct) = dereference_image(b, payload_store)?;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                let media = if ct.is_empty() { mime_type.clone() } else { ct };
                Some(json!({
                    "type":"input_image",
                    "image_url": format!("data:{};base64,{}", media, b64)
                }))
            }
            AgentMessageBlock::Thinking { .. } => {
                // Responses 渠道丢弃 thinking（spec §2: supports_thinking=false 时静默丢弃；
                // OpenAI 渠道当前不支持 inline thinking 回写）。
                None
            }
        })
    }

    fn message_to_input_items(
        &self,
        msg: &AgentMessage,
        payload_store: Option<&PayloadStore>,
    ) -> Result<Vec<Value>, WireMappingError> {
        let role = match msg.role {
            AgentMessageRole::System => return Ok(vec![]),
            AgentMessageRole::User => "user",
            AgentMessageRole::Assistant => "assistant",
        };
        let mut content = Vec::new();
        for b in &msg.blocks {
            if let Some(c) = self.block_to_input_content(b, msg.role, payload_store)? {
                content.push(c);
            }
        }
        let mut out = Vec::new();
        if !content.is_empty() {
            out.push(json!({
                "type":"message",
                "role": role,
                "content": content
            }));
        }
        Ok(out)
    }
}

impl ProviderAdapter for OpenAIResponsesAdapter {
    fn wire_format(&self) -> WireFormat {
        WireFormat::Responses
    }

    fn build_request_body(
        &self,
        msgs: &[AgentMessage],
        context: &ContextBundle,
        payload_store: Option<&PayloadStore>,
    ) -> Result<Value, WireMappingError> {
        let mut input: Vec<Value> = Vec::new();
        for m in msgs {
            m.validate_role_blocks()
                .map_err(|e| WireMappingError::InvalidMessage(format!("{:?}", e)))?;
            input.extend(self.message_to_input_items(m, payload_store)?);
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
        if let Some(m) = self.channel.max_output_tokens {
            body["max_output_tokens"] = Value::from(m);
        }
        // Spec §2: 不传 tools 字段、不传 server-side tool。
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
    use crate::domain::agent::WireFormat;
    use chrono::Utc;

    fn ch(vision: bool) -> ProviderChannel {
        ProviderChannel {
            channel_id: "oai-responses".into(),
            provider: "openai".into(),
            wire_format: WireFormat::Responses,
            base_url: None,
            api_key: String::new(),
            model: "gpt-5".into(),
            stream: true,
            enabled: true,
            supports_vision: vision,
            supports_thinking: false,
            max_output_tokens: Some(2048),
            context_window_tokens: Some(128_000),
            thinking_budget_tokens: None,
        }
    }

    fn msg(role: AgentMessageRole, blocks: Vec<AgentMessageBlock>) -> AgentMessage {
        AgentMessage {
            message_id: "m1".into(),
            run_id: Some("r1".into()),
            conversation_id: None,
            seq: None,
            kind: None,
            role,
            blocks,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn maps_user_message_to_input_text() {
        let ad = OpenAIResponsesAdapter::new(ch(false));
        let ctx = ContextBundle::new("r1");
        let msgs = vec![msg(
            AgentMessageRole::User,
            vec![AgentMessageBlock::Text { text: "hello".into() }],
        )];
        let body = ad.build_request_body(&msgs, &ctx, None).unwrap();
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
        assert!(body.get("tools").is_none());
        assert_eq!(body["max_output_tokens"], 2048);
    }

    #[test]
    fn build_request_body_reflects_multi_message_conversation() {
        // Regression: body must reflect the live growing messages slice passed by the loop, not a startup snapshot.
        let ad = OpenAIResponsesAdapter::new(ch(false));
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
        let arr = body["input"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        // User turns → input_text; assistant turn → output_text (Responses API requirement;
        // assistant input_text makes the upstream reject the whole multi-turn request).
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[0]["content"][0]["type"], "input_text");
        assert_eq!(arr[1]["role"], "assistant");
        assert_eq!(arr[1]["content"][0]["type"], "output_text");
        assert_eq!(arr[2]["role"], "user");
        assert_eq!(arr[2]["content"][0]["type"], "input_text");
        let last = arr[2]["content"][0]["text"].as_str().unwrap();
        assert!(last.contains("<skill_result"));
    }

    #[test]
    fn skill_xml_in_assistant_text_passes_through() {
        let ad = OpenAIResponsesAdapter::new(ch(false));
        let ctx = ContextBundle::new("r1");
        let msgs = vec![msg(
            AgentMessageRole::Assistant,
            vec![AgentMessageBlock::Text {
                text: r#"<use_skill name="fetch_quote">{"tsCode":"600519.SH"}</use_skill>"#.into(),
            }],
        )];
        let body = ad.build_request_body(&msgs, &ctx, None).unwrap();
        let item = &body["input"][0];
        assert_eq!(item["role"], "assistant");
        assert_eq!(item["content"][0]["type"], "output_text");
        let t = item["content"][0]["text"].as_str().unwrap();
        assert!(t.contains("<use_skill"));
    }

    #[test]
    fn vision_rejected_when_unsupported() {
        let ad = OpenAIResponsesAdapter::new(ch(false));
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
        let ad = OpenAIResponsesAdapter::new(ch(false));
        assert_eq!(ad.map_stop_reason("completed"), AgentStopReason::Completed);
        assert_eq!(
            ad.map_stop_reason("max_output_tokens"),
            AgentStopReason::MaxTurns
        );
    }
}
