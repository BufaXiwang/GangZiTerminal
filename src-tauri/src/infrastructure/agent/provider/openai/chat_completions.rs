//! OpenAI Chat Completions wire format（`/v1/chat/completions`）。
//!
//! 兼容性最广——OpenAI / DeepSeek / 火山方舟 / vLLM / Ollama 都照这个仿。
//! 局限是 OpenAI 内置 tools (web_search / file_search 等) 在这个 endpoint 不可用，
//! 也没有 Responses API 的高 cache 利用率优化。
//!
//! 关键差异 vs Anthropic Messages：
//! - tool 定义形态：`{type:"function", function:{name, description, parameters}}`
//! - tool_use → assistant 消息的 `tool_calls[]`，arguments 是 string 化的 JSON
//! - tool_result → 独立 `{role:"tool", tool_call_id, content}` 消息
//! - SSE 是单类型 chunk 流（不是 Anthropic 的命名事件），用 `data: [DONE]` 收尾
//! - 没有 thinking block，gpt-5 系列的 reasoning 内部消化

use super::common::{
    build_http_client, map_http_error, normalize_base_url, require_token, ReasoningEffort,
};
use crate::domain::agent::types::{
    AgentRequest, Block, Message, Role, ServerSideTool, StopReason, SystemBlock, ToolDef,
    ToolResultContent,
};
use crate::infrastructure::agent::provider::{
    ChatProvider, ProviderError, ProviderEvent, TokenUsage,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::stream::{self, BoxStream, StreamExt};
use reqwest::Client;
use serde_json::{json, Map, Value};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct OpenAIChatCompletionsConfig {
    pub base_url: String,
    pub token: String,
    pub request_timeout: Duration,
    /// gpt-5 系列设 Some(Medium)；gpt-4.1 / DeepSeek 等不识别该字段，保持 None。
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl OpenAIChatCompletionsConfig {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into(),
            request_timeout: Duration::from_secs(300),
            reasoning_effort: None,
        }
    }
}

#[derive(Debug)]
pub struct OpenAIChatCompletionsProvider {
    config: OpenAIChatCompletionsConfig,
    http: Client,
}

impl OpenAIChatCompletionsProvider {
    pub fn new(mut config: OpenAIChatCompletionsConfig) -> Result<Self, ProviderError> {
        require_token(&config.token)?;
        config.base_url = normalize_base_url(config.base_url)?;
        let http = build_http_client(config.request_timeout)?;
        Ok(Self { config, http })
    }

    fn build_request_body(&self, req: &AgentRequest) -> Value {
        let mut body = Map::new();
        body.insert("model".into(), json!(req.options.model));
        body.insert("max_tokens".into(), json!(req.options.max_tokens));
        body.insert("stream".into(), json!(true));
        body.insert("stream_options".into(), json!({"include_usage": true}));
        if let Some(t) = req.options.temperature {
            body.insert("temperature".into(), json!(t));
        }
        if let Some(p) = req.options.top_p {
            body.insert("top_p".into(), json!(p));
        }
        if !req.options.stop_sequences.is_empty() {
            body.insert("stop".into(), json!(req.options.stop_sequences));
        }
        if let Some(effort) = self.config.reasoning_effort {
            // gpt-5 系列在 Chat Completions 上接受 reasoning_effort 字段
            body.insert("reasoning_effort".into(), json!(effort.as_str()));
        }

        // messages = system 拼成一条 system 消息 + messages 翻译
        let mut wire_messages = Vec::new();
        if !req.system.is_empty() {
            wire_messages.push(json!({
                "role": "system",
                "content": system_blocks_to_text(&req.system),
            }));
        }
        for msg in &req.messages {
            wire_messages.extend(message_to_wire(msg));
        }
        body.insert("messages".into(), Value::Array(wire_messages));

        // tools
        let tools: Vec<Value> = req.tools.iter().filter_map(tool_def_to_wire).collect();
        if !tools.is_empty() {
            body.insert("tools".into(), Value::Array(tools));
        }

        Value::Object(body)
    }
}

#[async_trait]
impl ChatProvider for OpenAIChatCompletionsProvider {
    async fn stream(
        &self,
        req: &AgentRequest,
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError> {
        let body = self.build_request_body(req);
        let url = format!("{}/v1/chat/completions", self.config.base_url);
        let resp = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.config.token))
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|err| ProviderError::Transient(format!("openai 网络错误：{err}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(map_http_error(status.as_u16(), body_text));
        }

        let event_stream = resp.bytes_stream().eventsource();
        let translated = stream::unfold(
            (event_stream, ChatChunkDecoder::new()),
            |(mut es, mut decoder)| async move {
                loop {
                    match es.next().await {
                        None => {
                            if !decoder.completed {
                                return Some((
                                    Err(ProviderError::Protocol(
                                        "openai chat 流提前结束，没有 [DONE]".into(),
                                    )),
                                    (es, decoder),
                                ));
                            }
                            return None;
                        }
                        Some(Err(err)) => {
                            return Some((
                                Err(ProviderError::Protocol(format!("SSE 解析失败：{err}"))),
                                (es, decoder),
                            ));
                        }
                        Some(Ok(ev)) => match decoder.consume(&ev.data) {
                            Ok(Some(out)) => return Some((Ok(out), (es, decoder))),
                            Ok(None) => continue,
                            Err(err) => return Some((Err(err), (es, decoder))),
                        },
                    }
                }
            },
        );
        Ok(translated.boxed())
    }
}

// ===== Wire format 翻译 ===================================================

fn system_blocks_to_text(blocks: &[SystemBlock]) -> String {
    blocks
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn tool_def_to_wire(tool: &ToolDef) -> Option<Value> {
    match tool {
        ToolDef::Local {
            name,
            description,
            input_schema,
            ..
        } => Some(json!({
            "type": "function",
            "function": {
                "name": name,
                "description": description,
                "parameters": input_schema,
            }
        })),
        // Anthropic 的 web_search 在 Chat Completions 没有等价物——丢弃。
        // 想要 web_search 的用户应该用 Responses provider。
        ToolDef::ServerSide(ServerSideTool::AnthropicWebSearch { .. }) => None,
    }
}

/// 一条 canonical Message 翻译成 1+ 条 OpenAI chat 消息。
/// 关键：tool_result 必须自成 role=tool 消息（不能跟其他 block 混在 user 消息里）。
fn message_to_wire(msg: &Message) -> Vec<Value> {
    let mut out = Vec::new();
    match msg.role {
        Role::User => {
            // 把 ToolResult 单独抽成 role=tool 消息；其余 block（Text/Image）合并成 user 消息
            let mut user_content: Vec<Value> = Vec::new();
            for block in &msg.content {
                match block {
                    Block::Text { text, .. } => {
                        user_content.push(json!({"type": "text", "text": text}));
                    }
                    Block::Image { mime, data } => {
                        user_content.push(json!({
                            "type": "image_url",
                            "image_url": {"url": format!("data:{mime};base64,{data}")},
                        }));
                    }
                    Block::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": tool_result_content_to_text(content),
                        }));
                    }
                    // 其他 block（thinking/redacted_thinking/tool_use）不该出现在 user
                    _ => {}
                }
            }
            if !user_content.is_empty() {
                // 单条 Text 直接用字符串形态——更兼容老的 OpenAI-clone
                let content_value = if user_content.len() == 1
                    && user_content[0]["type"].as_str() == Some("text")
                {
                    user_content[0]["text"].clone()
                } else {
                    Value::Array(user_content)
                };
                // 在 out 的开头插入 user 消息（OpenAI 要求 tool 消息紧跟在触发它的 assistant 之后；
                // 但若一个 user 消息同时含 text 和 tool_result，正确顺序是 [tool*, user]——
                // tool result 先回答上一个 assistant 的 tool_call，user 是新的 turn 输入）。
                // 这里反转直觉：tool_result 优先，user 文本作为 new turn 接在后面。
                out.push(json!({"role": "user", "content": content_value}));
            }
        }
        Role::Assistant => {
            let mut text_buf = String::new();
            let mut tool_calls = Vec::new();
            for block in &msg.content {
                match block {
                    Block::Text { text, .. } => text_buf.push_str(text),
                    Block::ToolUse {
                        id, name, input, ..
                    } => {
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": input.to_string(),
                            }
                        }));
                    }
                    // thinking 在 Chat Completions 没有等价 block——丢弃
                    _ => {}
                }
            }
            let mut msg_obj = Map::new();
            msg_obj.insert("role".into(), json!("assistant"));
            // content: 有文本就给文本，没文本但有 tool_calls 给 null
            if !text_buf.is_empty() {
                msg_obj.insert("content".into(), json!(text_buf));
            } else {
                msg_obj.insert("content".into(), Value::Null);
            }
            if !tool_calls.is_empty() {
                msg_obj.insert("tool_calls".into(), Value::Array(tool_calls));
            }
            out.push(Value::Object(msg_obj));
        }
    }
    out
}

fn tool_result_content_to_text(content: &[ToolResultContent]) -> String {
    // chat completions tool 消息只能是字符串。把多段 content 拼成一段。
    // Image 类型在 tool 消息里 OpenAI 不支持——降级为占位文案。
    let mut buf = String::new();
    for c in content {
        match c {
            ToolResultContent::Text { text } => buf.push_str(text),
            ToolResultContent::Image { mime, .. } => {
                buf.push_str(&format!("[image/{mime} omitted: tool 消息不支持图片]"))
            }
            ToolResultContent::Json { raw } => buf.push_str(&raw.to_string()),
        }
        buf.push('\n');
    }
    buf.trim_end().to_string()
}

// ===== SSE chunk 解码器 ==================================================

/// 累积流式 chunk，在收到 finish_reason 或 usage 时 emit 对应 ProviderEvent。
struct ChatChunkDecoder {
    /// assistant 文本逐 token 拼接
    text_buf: String,
    /// 流式 tool_calls 收集——key 是 chunk 给的 index（不是最终 id）
    tool_calls: Vec<PartialToolCall>,
    finish_reason: Option<String>,
    completed: bool,
}

#[derive(Debug, Default, Clone)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl ChatChunkDecoder {
    fn new() -> Self {
        Self {
            text_buf: String::new(),
            tool_calls: Vec::new(),
            finish_reason: None,
            completed: false,
        }
    }

    fn consume(&mut self, data: &str) -> Result<Option<ProviderEvent>, ProviderError> {
        // [DONE] 标志流结束——assemble final message
        if data.trim() == "[DONE]" {
            self.completed = true;
            return Ok(Some(self.assemble_final()?));
        }
        if data.is_empty() {
            return Ok(None);
        }
        let v: Value = serde_json::from_str(data)
            .map_err(|err| ProviderError::Protocol(format!("openai chunk 不是合法 JSON: {err}")))?;

        // usage chunk（最后一条；choices 为空）
        if let Some(usage) = v.get("usage").and_then(Value::as_object) {
            if !usage.is_empty() {
                let usage_evt = parse_usage(&Value::Object(usage.clone()));
                return Ok(Some(ProviderEvent::Usage(usage_evt)));
            }
        }

        let choices = match v.get("choices").and_then(Value::as_array) {
            Some(c) if !c.is_empty() => c,
            _ => return Ok(None),
        };
        let choice = &choices[0];
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        let delta = match choice.get("delta") {
            Some(d) => d,
            None => return Ok(None),
        };

        // 文本增量
        if let Some(content) = delta.get("content").and_then(Value::as_str) {
            if !content.is_empty() {
                self.text_buf.push_str(content);
                return Ok(Some(ProviderEvent::TextDelta(content.to_string())));
            }
        }

        // tool_calls 增量
        if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in tcs {
                let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                while self.tool_calls.len() <= idx {
                    self.tool_calls.push(PartialToolCall::default());
                }
                let slot = &mut self.tool_calls[idx];
                if let Some(id) = tc.get("id").and_then(Value::as_str) {
                    if !id.is_empty() {
                        slot.id = id.to_string();
                    }
                }
                if let Some(func) = tc.get("function") {
                    if let Some(name) = func.get("name").and_then(Value::as_str) {
                        if !name.is_empty() {
                            slot.name = name.to_string();
                        }
                    }
                    if let Some(args) = func.get("arguments").and_then(Value::as_str) {
                        slot.arguments.push_str(args);
                    }
                }
            }
        }
        Ok(None)
    }

    fn assemble_final(&mut self) -> Result<ProviderEvent, ProviderError> {
        let mut content: Vec<Block> = Vec::new();
        if !self.text_buf.is_empty() {
            content.push(Block::Text {
                text: std::mem::take(&mut self.text_buf),
                cache_control: false,
            });
        }
        for tc in std::mem::take(&mut self.tool_calls) {
            // arguments 是 string 化的 JSON——空字符串 = 无参数 = {}
            let input: Value = if tc.arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&tc.arguments).map_err(|err| {
                    ProviderError::Protocol(format!(
                        "tool_call arguments JSON 解析失败 (name={}, raw={}): {err}",
                        tc.name, tc.arguments
                    ))
                })?
            };
            content.push(Block::ToolUse {
                id: tc.id,
                name: tc.name,
                input,
                server_side: false,
            });
        }
        let stop_reason = match self.finish_reason.as_deref() {
            Some("stop") => StopReason::EndTurn,
            Some("length") => StopReason::MaxTokens,
            Some("tool_calls") | Some("function_call") => StopReason::EndTurn,
            Some("content_filter") => StopReason::Refusal,
            _ => StopReason::EndTurn,
        };
        Ok(ProviderEvent::MessageComplete {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason,
        })
    }
}

fn parse_usage(v: &Value) -> TokenUsage {
    let cached = v
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    TokenUsage {
        input_tokens: v.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
        output_tokens: v
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        cache_read_tokens: cached,
        // OpenAI 没有"cache write"概念——cache 是 implicit 的，不区分写入。
        cache_write_tokens: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::types::{AgentOptions, ContextBudget, PipelineKind};

    fn dummy_request() -> AgentRequest {
        AgentRequest {
            system: vec![SystemBlock {
                text: "you are a stock assistant".into(),
                cache_control: false,
            }],
            tools: vec![
                ToolDef::Local {
                    name: "get_quote".into(),
                    description: "拉行情".into(),
                    input_schema: json!({"type": "object"}),
                    cache_control: false,
                },
                // 这条在 Chat Completions 应该被丢弃
                ToolDef::ServerSide(ServerSideTool::AnthropicWebSearch {
                    name: "web_search".into(),
                    max_uses: Some(5),
                    allowed_domains: vec![],
                    blocked_domains: vec![],
                }),
            ],
            messages: vec![Message {
                role: Role::User,
                content: vec![Block::Text {
                    text: "茅台怎么样？".into(),
                    cache_control: false,
                }],
            }],
            options: AgentOptions {
                model: "gpt-5.5".into(),
                max_tokens: 1024,
                temperature: Some(0.5),
                top_p: None,
                thinking: None,
                effort: None,
                max_turns: 10,
                stop_sequences: vec!["</end>".into()],
                tool_timeout_secs: None,
            },
            budget: ContextBudget {
                soft_limit_tokens: 80_000,
                hard_limit_tokens: 160_000,
                compact_keep_last_n: 6,
                max_search_calls: 5,
            },
            trigger_message_id: None,
            pipeline: PipelineKind::Chat,
        }
    }

    #[test]
    fn build_request_body_basic_shape() {
        let provider = OpenAIChatCompletionsProvider::new(OpenAIChatCompletionsConfig::new(
            "https://api.openai.com",
            "sk-test",
        ))
        .unwrap();
        let body = provider.build_request_body(&dummy_request());
        assert_eq!(body["model"], "gpt-5.5");
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["stop"][0], "</end>");

        // system 拼成单条 system 消息
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "you are a stock assistant");
        assert_eq!(msgs[1]["role"], "user");

        // tools：本地工具序列化成 function 形态；server-side web_search 被丢
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "get_quote");
    }

    #[test]
    fn reasoning_effort_sent_when_set() {
        let mut cfg = OpenAIChatCompletionsConfig::new("https://api.openai.com", "sk-test");
        cfg.reasoning_effort = Some(ReasoningEffort::Medium);
        let provider = OpenAIChatCompletionsProvider::new(cfg).unwrap();
        let body = provider.build_request_body(&dummy_request());
        assert_eq!(body["reasoning_effort"], "medium");
    }

    #[test]
    fn reasoning_effort_absent_by_default() {
        let provider = OpenAIChatCompletionsProvider::new(OpenAIChatCompletionsConfig::new(
            "https://api.openai.com",
            "sk-test",
        ))
        .unwrap();
        let body = provider.build_request_body(&dummy_request());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn tool_use_in_assistant_msg_serializes_with_string_arguments() {
        let provider = OpenAIChatCompletionsProvider::new(OpenAIChatCompletionsConfig::new(
            "https://api.openai.com",
            "sk-test",
        ))
        .unwrap();
        let mut req = dummy_request();
        req.messages.push(Message {
            role: Role::Assistant,
            content: vec![
                Block::Text {
                    text: "我查一下".into(),
                    cache_control: false,
                },
                Block::ToolUse {
                    id: "call_42".into(),
                    name: "get_quote".into(),
                    input: json!({"code": "600519"}),
                    server_side: false,
                },
            ],
        });
        let body = provider.build_request_body(&req);
        let last = body["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last["role"], "assistant");
        assert_eq!(last["content"], "我查一下");
        let tool_call = &last["tool_calls"][0];
        assert_eq!(tool_call["id"], "call_42");
        assert_eq!(tool_call["function"]["name"], "get_quote");
        // arguments 必须是 string 不是 object
        let args = tool_call["function"]["arguments"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(args).unwrap();
        assert_eq!(parsed["code"], "600519");
    }

    #[test]
    fn tool_result_becomes_role_tool_message() {
        let provider = OpenAIChatCompletionsProvider::new(OpenAIChatCompletionsConfig::new(
            "https://api.openai.com",
            "sk-test",
        ))
        .unwrap();
        let mut req = dummy_request();
        req.messages.push(Message {
            role: Role::User,
            content: vec![Block::ToolResult {
                tool_use_id: "call_42".into(),
                content: vec![ToolResultContent::Text {
                    text: "1888 元".into(),
                }],
                is_error: false,
                server_side: false,
                cache_control: false,
            }],
        });
        let body = provider.build_request_body(&req);
        let msgs = body["messages"].as_array().unwrap();
        let tool_msg = msgs.iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(tool_msg["tool_call_id"], "call_42");
        assert_eq!(tool_msg["content"], "1888 元");
    }

    #[test]
    fn image_block_serializes_as_data_url() {
        let provider = OpenAIChatCompletionsProvider::new(OpenAIChatCompletionsConfig::new(
            "https://api.openai.com",
            "sk-test",
        ))
        .unwrap();
        let mut req = dummy_request();
        req.messages = vec![Message {
            role: Role::User,
            content: vec![
                Block::Text {
                    text: "see this".into(),
                    cache_control: false,
                },
                Block::Image {
                    mime: "image/png".into(),
                    data: "iVBORw0".into(),
                },
            ],
        }];
        let body = provider.build_request_body(&req);
        let user_msg = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "user")
            .unwrap();
        let parts = user_msg["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        let url = parts[1]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,iVBORw0"));
    }

    #[test]
    fn rejects_empty_token() {
        let err =
            OpenAIChatCompletionsProvider::new(OpenAIChatCompletionsConfig::new("https://x", ""))
                .unwrap_err();
        assert!(matches!(err, ProviderError::Config(_)));
    }

    #[test]
    fn thinking_block_dropped_in_chat() {
        // canonical 上有 Thinking——chat completions 没等价物，应该被丢弃
        let provider = OpenAIChatCompletionsProvider::new(OpenAIChatCompletionsConfig::new(
            "https://api.openai.com",
            "sk-test",
        ))
        .unwrap();
        let mut req = dummy_request();
        req.messages.push(Message {
            role: Role::Assistant,
            content: vec![
                Block::Thinking {
                    thinking: "should not leak".into(),
                    signature: None,
                },
                Block::Text {
                    text: "actual answer".into(),
                    cache_control: false,
                },
            ],
        });
        let body = provider.build_request_body(&req);
        let body_str = serde_json::to_string(&body).unwrap();
        assert!(
            !body_str.contains("should not leak"),
            "thinking 内容必须不出现在请求里"
        );
        assert!(body_str.contains("actual answer"));
    }

    // ===== SSE chunk decoder =====

    #[test]
    fn chunk_decoder_emits_text_delta_then_complete() {
        let mut dec = ChatChunkDecoder::new();
        let ev = dec
            .consume(r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"你"}}]}"#)
            .unwrap();
        match ev {
            Some(ProviderEvent::TextDelta(s)) => assert_eq!(s, "你"),
            _ => panic!(),
        }
        let _ = dec
            .consume(r#"{"choices":[{"index":0,"delta":{"content":"好"},"finish_reason":null}]}"#);
        let _ = dec.consume(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#);
        let usage_ev = dec
            .consume(
                r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":3}}}"#,
            )
            .unwrap();
        match usage_ev {
            Some(ProviderEvent::Usage(u)) => {
                assert_eq!(u.input_tokens, 10);
                assert_eq!(u.output_tokens, 2);
                assert_eq!(u.cache_read_tokens, 3);
            }
            _ => panic!(),
        }
        let final_ev = dec.consume("[DONE]").unwrap();
        match final_ev {
            Some(ProviderEvent::MessageComplete {
                message,
                stop_reason,
            }) => {
                assert_eq!(stop_reason, StopReason::EndTurn);
                match &message.content[0] {
                    Block::Text { text, .. } => assert_eq!(text, "你好"),
                    _ => panic!(),
                }
            }
            _ => panic!(),
        }
    }

    #[test]
    fn chunk_decoder_assembles_streamed_tool_call() {
        let mut dec = ChatChunkDecoder::new();
        // 第一个 chunk：id + name 给齐
        let _ = dec.consume(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_quote","arguments":"{\"co"}}]}}]}"#,
        );
        // 后续 arguments 增量
        let _ = dec.consume(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"de\":\"600519\"}"}}]}}]}"#,
        );
        // finish
        let _ = dec.consume(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#);
        let final_ev = dec.consume("[DONE]").unwrap();
        match final_ev {
            Some(ProviderEvent::MessageComplete { message, .. }) => {
                let tu = &message.content[0];
                match tu {
                    Block::ToolUse {
                        id, name, input, ..
                    } => {
                        assert_eq!(id, "call_1");
                        assert_eq!(name, "get_quote");
                        assert_eq!(input["code"], "600519");
                    }
                    _ => panic!(),
                }
            }
            _ => panic!(),
        }
    }

    #[test]
    fn chunk_decoder_maps_length_to_max_tokens() {
        let mut dec = ChatChunkDecoder::new();
        let _ = dec.consume(
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"halfway"},"finish_reason":null}]}"#,
        );
        let _ = dec.consume(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#);
        let final_ev = dec.consume("[DONE]").unwrap();
        match final_ev {
            Some(ProviderEvent::MessageComplete { stop_reason, .. }) => {
                assert_eq!(stop_reason, StopReason::MaxTokens);
            }
            _ => panic!(),
        }
    }
}
