//! Anthropic Messages channel adapter — 纯 chat（无 tool_use）。
//!
//! Spec: docs/design/agent-infra-module.md §2 ProviderChannel
//!       docs/design/references/agent/anthropic-messages.md
//!
//! 行为：
//! - 把 canonical `AgentMessage` 翻译成 Anthropic `/v1/messages` 请求 body。
//! - 图片 `dataRef` 在 build wire 时从 PayloadStore dereference → base64 编码。
//! - Thinking block 把 `metadata` 中的 `signature` / `redacted` 还原回 wire。
//! - **不**传 tools 字段、**不**解析 tool_use block（tool 走文本协议）。

use super::{dereference_image, ProviderAdapter, WireMappingError};
use crate::domain::agent::{
    AgentMessage, AgentMessageBlock, AgentMessageRole, AgentStopReason, ContextBundle,
    ProviderChannel, WireFormat,
};
use crate::domain::agent::context::ContextContent;
use crate::infrastructure::agent::payload_store::PayloadStore;
use base64::Engine;
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
            .map(|p| match &p.content {
                ContextContent::Text(s) => s.clone(),
                ContextContent::Json(v) => v.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn block_to_anthropic(
        &self,
        b: &AgentMessageBlock,
        payload_store: Option<&PayloadStore>,
    ) -> Result<Option<Value>, WireMappingError> {
        Ok(match b {
            AgentMessageBlock::Text { text } => {
                Some(json!({ "type": "text", "text": text }))
            }
            AgentMessageBlock::Image { mime_type, .. } => {
                if !self.channel.supports_vision {
                    return Err(WireMappingError::VisionNotSupported);
                }
                let (bytes, ct) = dereference_image(b, payload_store)?;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                let media_type = if ct.is_empty() { mime_type.clone() } else { ct };
                Some(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": media_type,
                        "data": b64
                    }
                }))
            }
            AgentMessageBlock::Thinking {
                text,
                provider: _,
                metadata,
            } => {
                if !self.channel.supports_thinking {
                    return Ok(None);
                }
                // redacted thinking is its own block shape.
                // Per anthropic-messages.md:
                //   normal   = {type:"thinking", thinking, signature}
                //   redacted = {type:"redacted_thinking", data}
                // Trigger redaction ONLY on a definite signal — `type=="redacted_thinking"`
                // or the dedicated `redacted` key. A bare `data` field must NOT flip a normal
                // thinking block to redacted (that over-broad trigger was a latent footgun).
                let redacted_data = metadata.as_ref().and_then(|m| {
                    let is_redacted_type = m
                        .get("type")
                        .and_then(|t| t.as_str())
                        .map(|t| t == "redacted_thinking")
                        .unwrap_or(false);
                    let redacted_blob = m.get("redacted").and_then(|d| d.as_str());
                    if !is_redacted_type && redacted_blob.is_none() {
                        return None; // normal thinking
                    }
                    // redacted: prefer the encrypted blob (`redacted` or `data`), else fall back
                    // to the thinking text so we still emit a well-formed redacted_thinking block.
                    Some(
                        redacted_blob
                            .or_else(|| m.get("data").and_then(|d| d.as_str()))
                            .map(|d| d.to_string())
                            .unwrap_or_else(|| text.clone()),
                    )
                });

                if let Some(data) = redacted_data {
                    Some(json!({
                        "type": "redacted_thinking",
                        "data": data,
                    }))
                } else {
                    let mut obj = serde_json::Map::new();
                    obj.insert("type".into(), json!("thinking"));
                    obj.insert("thinking".into(), json!(text));
                    if let Some(m) = metadata {
                        if let Some(sig) = m.get("signature") {
                            obj.insert("signature".into(), sig.clone());
                        }
                    }
                    Some(Value::Object(obj))
                }
            }
        })
    }

    fn message_to_anthropic(
        &self,
        msg: &AgentMessage,
        payload_store: Option<&PayloadStore>,
    ) -> Result<Option<Value>, WireMappingError> {
        let role = match msg.role {
            // system 走 top-level `system`
            AgentMessageRole::System => return Ok(None),
            AgentMessageRole::User => "user",
            AgentMessageRole::Assistant => "assistant",
        };
        let mut content = Vec::new();
        for b in &msg.blocks {
            if let Some(v) = self.block_to_anthropic(b, payload_store)? {
                content.push(v);
            }
        }
        if content.is_empty() {
            return Ok(None);
        }
        Ok(Some(json!({
            "role": role,
            "content": content
        })))
    }
}

impl ProviderAdapter for AnthropicAdapter {
    fn wire_format(&self) -> WireFormat {
        WireFormat::Messages
    }

    fn build_request_body(
        &self,
        msgs: &[AgentMessage],
        context: &ContextBundle,
        payload_store: Option<&PayloadStore>,
    ) -> Result<Value, WireMappingError> {
        let system_text = Self::extract_system(context);
        let mut messages = Vec::with_capacity(msgs.len());
        for m in msgs {
            m.validate_role_blocks()
                .map_err(|e| WireMappingError::InvalidMessage(format!("{:?}", e)))?;
            if let Some(v) = self.message_to_anthropic(m, payload_store)? {
                messages.push(v);
            }
        }
        let max_tokens = self.channel.max_output_tokens.unwrap_or(8192);
        let mut body = json!({
            "model": self.channel.model,
            "stream": self.channel.stream,
            "max_tokens": max_tokens,
            "messages": messages,
        });
        if !system_text.is_empty() {
            body["system"] = Value::String(system_text);
        }
        // request-level extended-thinking config. Only emit when the channel both
        // supports thinking and has an explicit budget. Default (no budget) = no thinking,
        // preserving prior behavior. Per anthropic-messages.md: budget_tokens must be
        // ≥ 1024 and < max_tokens.
        if self.channel.supports_thinking {
            if let Some(budget) = self.channel.thinking_budget_tokens {
                if budget < 1024 {
                    return Err(WireMappingError::Unsupported(
                        WireFormat::Messages,
                        format!("thinking budget_tokens {budget} must be >= 1024"),
                    ));
                }
                if budget >= max_tokens {
                    return Err(WireMappingError::Unsupported(
                        WireFormat::Messages,
                        format!(
                            "thinking budget_tokens {budget} must be < max_tokens {max_tokens}"
                        ),
                    ));
                }
                body["thinking"] = json!({
                    "type": "enabled",
                    "budget_tokens": budget,
                });
            }
        }
        Ok(body)
    }

    fn map_stop_reason(&self, raw: &str) -> AgentStopReason {
        match raw {
            "end_turn" => AgentStopReason::Completed,
            "max_tokens" => AgentStopReason::MaxTurns,
            "stop_sequence" => AgentStopReason::ProviderStop,
            _ => AgentStopReason::Completed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::{AgentMessage, AgentMessageBlock, AgentMessageRole, WireFormat};
    use chrono::Utc;

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

    fn make_channel(vision: bool, thinking: bool) -> ProviderChannel {
        ProviderChannel {
            channel_id: "anthropic".into(),
            provider: "anthropic".into(),
            wire_format: WireFormat::Messages,
            base_url: None,
            api_key: String::new(),
            model: "claude-sonnet-4-5".into(),
            stream: true,
            enabled: true,
            supports_vision: vision,
            supports_thinking: thinking,
            max_output_tokens: Some(4096),
            context_window_tokens: Some(200_000),
            thinking_budget_tokens: None,
        }
    }

    #[test]
    fn build_request_body_basic_chat_no_tools_field() {
        let ad = AnthropicAdapter::new(make_channel(false, false));
        let mut ctx = ContextBundle::new("r1");
        ctx.system_parts.push(crate::domain::agent::ContextPart {
            kind: crate::domain::agent::ContextPartKind::System,
            content: ContextContent::Text("You are Gangzi.".into()),
            freshness: None,
            token_estimate: None,
            droppable: false,
        });
        let msgs = vec![msg(
            AgentMessageRole::User,
            vec![AgentMessageBlock::Text {
                text: "plan trade".into(),
            }],
        )];
        let body = ad.build_request_body(&msgs, &ctx, None).unwrap();
        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["system"], "You are Gangzi.");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert_eq!(body["max_tokens"], 4096);
        // Spec §2: provider request 不传 tools 字段。
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn build_request_body_reflects_multi_message_conversation() {
        // Regression: body must reflect the live growing messages slice passed by the loop,
        // not a startup snapshot — incl <tool_result> user messages.
        let ad = AnthropicAdapter::new(make_channel(false, false));
        let ctx = ContextBundle::new("r1");
        let msgs = vec![
            msg(
                AgentMessageRole::User,
                vec![AgentMessageBlock::Text { text: "查行情".into() }],
            ),
            msg(
                AgentMessageRole::Assistant,
                vec![AgentMessageBlock::Text {
                    text: r#"<use_tool name="fetch_quote">{"tsCode":"600519.SH"}</use_tool>"#.into(),
                }],
            ),
            msg(
                AgentMessageRole::User,
                vec![AgentMessageBlock::Text {
                    text: r#"<tool_result name="fetch_quote" call_id="tc_1">{"price":"1820"}</tool_result>"#.into(),
                }],
            ),
        ];
        let body = ad.build_request_body(&msgs, &ctx, None).unwrap();
        let arr = body["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[1]["role"], "assistant");
        assert_eq!(arr[2]["role"], "user");
        let last = arr[2]["content"][0]["text"].as_str().unwrap();
        assert!(last.contains("<tool_result"));
    }

    #[test]
    fn tool_call_xml_round_trips_as_text_block() {
        let ad = AnthropicAdapter::new(make_channel(false, false));
        let ctx = ContextBundle::new("r1");
        let msgs = vec![msg(
            AgentMessageRole::Assistant,
            vec![AgentMessageBlock::Text {
                text: r#"<use_tool name="fetch_quote">{"tsCode":"600519.SH"}</use_tool>"#.into(),
            }],
        )];
        let body = ad.build_request_body(&msgs, &ctx, None).unwrap();
        assert_eq!(body["messages"][0]["role"], "assistant");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        let s = body["messages"][0]["content"][0]["text"].as_str().unwrap();
        assert!(s.contains("<use_tool"));
    }

    #[test]
    fn image_block_rejected_when_vision_unsupported() {
        let ad = AnthropicAdapter::new(make_channel(false, false));
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
    fn thinking_silently_dropped_when_unsupported() {
        let ad = AnthropicAdapter::new(make_channel(false, false));
        let ctx = ContextBundle::new("r1");
        let msgs = vec![msg(
            AgentMessageRole::Assistant,
            vec![
                AgentMessageBlock::Thinking {
                    text: "reasoning".into(),
                    provider: None,
                    metadata: None,
                },
                AgentMessageBlock::Text { text: "ok".into() },
            ],
        )];
        let body = ad.build_request_body(&msgs, &ctx, None).unwrap();
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
    }

    #[test]
    fn thinking_with_signature_preserved_when_supported() {
        let ad = AnthropicAdapter::new(make_channel(false, true));
        let ctx = ContextBundle::new("r1");
        let msgs = vec![msg(
            AgentMessageRole::Assistant,
            vec![AgentMessageBlock::Thinking {
                text: "reasoning".into(),
                provider: Some("anthropic".into()),
                metadata: Some(json!({"signature":"sig-xyz"})),
            }],
        )];
        let body = ad.build_request_body(&msgs, &ctx, None).unwrap();
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "thinking");
        assert_eq!(block["thinking"], "reasoning");
        assert_eq!(block["signature"], "sig-xyz");
    }

    #[test]
    fn map_stop_reason_known_codes() {
        let ad = AnthropicAdapter::new(make_channel(false, false));
        assert_eq!(ad.map_stop_reason("end_turn"), AgentStopReason::Completed);
        assert_eq!(ad.map_stop_reason("max_tokens"), AgentStopReason::MaxTurns);
        assert_eq!(
            ad.map_stop_reason("stop_sequence"),
            AgentStopReason::ProviderStop
        );
    }

    // redacted thinking serializes as {type:"redacted_thinking", data}, NOT a
    // `redacted` field inside a `thinking` block.
    #[test]
    fn redacted_thinking_uses_redacted_thinking_block() {
        let ad = AnthropicAdapter::new(make_channel(false, true));
        let ctx = ContextBundle::new("r1");
        let msgs = vec![msg(
            AgentMessageRole::Assistant,
            vec![AgentMessageBlock::Thinking {
                text: "ignored".into(),
                provider: Some("anthropic".into()),
                metadata: Some(json!({"type":"redacted_thinking","data":"enc-blob-123"})),
            }],
        )];
        let body = ad.build_request_body(&msgs, &ctx, None).unwrap();
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "redacted_thinking");
        assert_eq!(block["data"], "enc-blob-123");
        assert!(block.get("thinking").is_none());
        assert!(block.get("redacted").is_none());
    }

    // a bare `redacted` blob (legacy metadata) also maps to redacted_thinking.
    #[test]
    fn redacted_metadata_blob_maps_to_redacted_thinking() {
        let ad = AnthropicAdapter::new(make_channel(false, true));
        let ctx = ContextBundle::new("r1");
        let msgs = vec![msg(
            AgentMessageRole::Assistant,
            vec![AgentMessageBlock::Thinking {
                text: "ignored".into(),
                provider: None,
                metadata: Some(json!({"redacted":"enc-xyz"})),
            }],
        )];
        let body = ad.build_request_body(&msgs, &ctx, None).unwrap();
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "redacted_thinking");
        assert_eq!(block["data"], "enc-xyz");
    }

    // default (no budget) → no request-level thinking config.
    #[test]
    fn no_request_thinking_config_by_default() {
        let ad = AnthropicAdapter::new(make_channel(false, true));
        let ctx = ContextBundle::new("r1");
        let msgs = vec![msg(
            AgentMessageRole::User,
            vec![AgentMessageBlock::Text { text: "hi".into() }],
        )];
        let body = ad.build_request_body(&msgs, &ctx, None).unwrap();
        assert!(body.get("thinking").is_none());
    }

    // budget + supports_thinking → top-level thinking:{type:enabled,budget_tokens}.
    #[test]
    fn request_thinking_config_emitted_with_budget() {
        let mut ch = make_channel(false, true);
        ch.max_output_tokens = Some(4096);
        ch.thinking_budget_tokens = Some(2048);
        let ad = AnthropicAdapter::new(ch);
        let ctx = ContextBundle::new("r1");
        let msgs = vec![msg(
            AgentMessageRole::User,
            vec![AgentMessageBlock::Text { text: "hi".into() }],
        )];
        let body = ad.build_request_body(&msgs, &ctx, None).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 2048);
    }

    // budget >= max_tokens → error (must be < max_tokens).
    #[test]
    fn request_thinking_budget_must_be_less_than_max_tokens() {
        let mut ch = make_channel(false, true);
        ch.max_output_tokens = Some(2048);
        ch.thinking_budget_tokens = Some(4096);
        let ad = AnthropicAdapter::new(ch);
        let ctx = ContextBundle::new("r1");
        let msgs = vec![msg(
            AgentMessageRole::User,
            vec![AgentMessageBlock::Text { text: "hi".into() }],
        )];
        let err = ad.build_request_body(&msgs, &ctx, None).unwrap_err();
        assert!(matches!(err, WireMappingError::Unsupported(..)));
    }
}
