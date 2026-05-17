//! OpenAI Responses API（`/v1/responses`）。
//!
//! 这是 OpenAI 当前主推的 agent endpoint：input 是 typed items 数组、tool 定义内部
//! tagged、cache 利用率比 Chat Completions 高 40-80%。GPT-5.5 / o3 系列推荐走这条。
//!
//! 请求形态（精简）：
//! ```json
//! {
//!   "model": "gpt-5.5",
//!   "input": [
//!     {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "..."}]},
//!     {"type": "function_call", "call_id": "fc_1", "name": "get_quote", "arguments": "{...}"},
//!     {"type": "function_call_output", "call_id": "fc_1", "output": "..."}
//!   ],
//!   "tools": [{"type": "function", "name": "get_quote", "parameters": {...}, "strict": false}],
//!   "stream": true,
//!   "reasoning": {"effort": "medium"}
//! }
//! ```
//!
//! SSE 事件形态：每条 SSE 都有命名 event（`response.output_text.delta` 等），data 是
//! 该事件的 JSON payload。我们关心的：
//! - `response.output_text.delta` → TextDelta
//! - `response.output_item.added` → 创建一个新 item（function_call 收 id+name）
//! - `response.function_call_arguments.delta` → 累积 tool args
//! - `response.output_item.done` → 关闭一个 item，把它推入 final message
//! - `response.completed` → 整次响应结束（usage 在 payload.response.usage 里）
//! - `response.failed` / `response.error` → 错误

use super::common::{
    build_http_client, map_http_error, normalize_base_url, require_token, ReasoningEffort,
};
use crate::infrastructure::agent::provider::{ChatProvider, ProviderError, ProviderEvent, TokenUsage};
use crate::domain::agent::types::{
    AgentRequest, Block, Message, Role, ServerSideTool, StopReason, SystemBlock, ToolDef,
    ToolResultContent,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::stream::{self, BoxStream, StreamExt};
use reqwest::Client;
use serde_json::{json, Map, Value};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct OpenAIResponsesConfig {
    pub base_url: String,
    pub token: String,
    pub request_timeout: Duration,
    pub reasoning_effort: Option<ReasoningEffort>,
    /// 启用内置 web_search tool。Anthropic 的 web_search server-side tool 会被翻译过来。
    pub enable_web_search: bool,
}

impl OpenAIResponsesConfig {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into(),
            request_timeout: Duration::from_secs(300),
            reasoning_effort: None,
            enable_web_search: false,
        }
    }
}

#[derive(Debug)]
pub struct OpenAIResponsesProvider {
    config: OpenAIResponsesConfig,
    http: Client,
}

impl OpenAIResponsesProvider {
    pub fn new(mut config: OpenAIResponsesConfig) -> Result<Self, ProviderError> {
        require_token(&config.token)?;
        config.base_url = normalize_base_url(config.base_url)?;
        let http = build_http_client(config.request_timeout)?;
        Ok(Self { config, http })
    }

    fn build_request_body(&self, req: &AgentRequest) -> Value {
        let mut body = Map::new();
        body.insert("model".into(), json!(req.options.model));
        body.insert("max_output_tokens".into(), json!(req.options.max_tokens));
        body.insert("stream".into(), json!(true));
        if let Some(t) = req.options.temperature {
            body.insert("temperature".into(), json!(t));
        }
        if let Some(p) = req.options.top_p {
            body.insert("top_p".into(), json!(p));
        }
        if let Some(effort) = self.config.reasoning_effort {
            body.insert("reasoning".into(), json!({"effort": effort.as_str()}));
        }
        // system → instructions（Responses 把 system 单独放到 instructions 字段）
        if !req.system.is_empty() {
            body.insert(
                "instructions".into(),
                json!(system_blocks_to_text(&req.system)),
            );
        }
        // input：把 messages 翻译成 typed Item 数组
        let mut items = Vec::new();
        for msg in &req.messages {
            items.extend(message_to_items(msg));
        }
        body.insert("input".into(), Value::Array(items));
        // tools
        let mut tools: Vec<Value> = req.tools.iter().filter_map(tool_def_to_wire).collect();
        if self.config.enable_web_search {
            // 即使 canonical 没声明 server-side web_search，配置里开了就加进去。
            // 重复时去重——上面 filter_map 已经从 ServerSide(WebSearch) 翻译了一次。
            if !tools
                .iter()
                .any(|t| t.get("type").and_then(Value::as_str) == Some("web_search"))
            {
                tools.push(json!({"type": "web_search"}));
            }
        }
        if !tools.is_empty() {
            body.insert("tools".into(), Value::Array(tools));
        }
        Value::Object(body)
    }
}

#[async_trait]
impl ChatProvider for OpenAIResponsesProvider {
    async fn stream(
        &self,
        req: &AgentRequest,
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError> {
        let body = self.build_request_body(req);
        let url = format!("{}/v1/responses", self.config.base_url);
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
            (event_stream, ResponsesDecoder::new()),
            |(mut es, mut decoder)| async move {
                loop {
                    // 流自然结束时如果还有暂存的 final，先发出去再 None。
                    // 这是 response.completed 已经触发 emit Usage 但 final 没机会出去的兜底路径。
                    match es.next().await {
                        None => {
                            if let Some(final_ev) = decoder.pending_final.take() {
                                return Some((Ok(final_ev), (es, decoder)));
                            }
                            if !decoder.completed {
                                // 软容错：relay / 网络偶发会丢掉最后的 `response.completed` 事件，
                                // 但前面所有 output_item 已经齐全。这种情况下硬报错不友好，
                                // 用现有的 items 软组装最终消息（usage / 精确 stop_reason 丢失，
                                // 接受这个降级——比让用户看"对话失败"好）。
                                if !decoder.items.is_empty() {
                                    tracing::warn!(
                                        "openai responses 流缺 response.completed 事件，按软成功降级（基于已收到的 output_item 组装）"
                                    );
                                    decoder.completed = true; // 防止下次循环再次进入这条分支
                                    match decoder.assemble_final() {
                                        Ok(final_ev) => {
                                            return Some((Ok(final_ev), (es, decoder)));
                                        }
                                        Err(_) => {
                                            // assemble 失败（如 function_call args 不完整）→ 仍按硬错
                                        }
                                    }
                                }
                                return Some((
                                    Err(ProviderError::Protocol(
                                        "openai responses 流提前结束（没收到 response.completed 且无可用 item）".into(),
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
                        Some(Ok(ev)) => match decoder.consume(ev.event.as_str(), &ev.data) {
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
            "name": name,
            "description": description,
            "parameters": input_schema,
            // strict=false：我们的工具 schema 没都按 strict 要求写（additionalProperties 等）
            "strict": false,
        })),
        ToolDef::ServerSide(ServerSideTool::AnthropicWebSearch { .. }) => {
            // Anthropic web_search → OpenAI built-in web_search
            // allowed_domains / blocked_domains 在 OpenAI 没有等价配置——v1 丢弃
            Some(json!({"type": "web_search"}))
        }
    }
}

/// canonical Message → 1+ Responses Items。
///
/// User text/image → message item；ToolResult → function_call_output item。
/// Assistant text → message item；ToolUse → function_call item。
fn message_to_items(msg: &Message) -> Vec<Value> {
    let mut out = Vec::new();
    match msg.role {
        Role::User => {
            let mut parts: Vec<Value> = Vec::new();
            for block in &msg.content {
                match block {
                    Block::Text { text, .. } => {
                        parts.push(json!({"type": "input_text", "text": text}));
                    }
                    Block::Image { mime, data } => {
                        parts.push(json!({
                            "type": "input_image",
                            "image_url": format!("data:{mime};base64,{data}"),
                        }));
                    }
                    Block::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        out.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": tool_result_content_to_text(content),
                        }));
                    }
                    _ => {}
                }
            }
            if !parts.is_empty() {
                out.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": parts,
                }));
            }
        }
        Role::Assistant => {
            // 关键：必须按原始 block 顺序 emit Items。
            // 原本"先收集所有 Text 到 parts，function_call 直接 push 到 out，
            // 最后再 push message"——会让 wire 顺序变成
            // [function_call, message]，但 canonical 顺序是 [Text..., ToolUse]，
            // 语义反了（"先调工具再说话" vs "先说话再调工具"）。
            //
            // 改成：遇到 ToolUse 时先 flush 累计的 Text 到一条 message item，
            // 再 push function_call；末尾若还有累计 Text 再 flush 一条。
            let mut pending_text: Vec<Value> = Vec::new();
            for block in &msg.content {
                match block {
                    Block::Text { text, .. } => {
                        pending_text.push(json!({"type": "output_text", "text": text}));
                    }
                    Block::ToolUse {
                        id, name, input, ..
                    } => {
                        if !pending_text.is_empty() {
                            out.push(json!({
                                "type": "message",
                                "role": "assistant",
                                "content": std::mem::take(&mut pending_text),
                            }));
                        }
                        out.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": input.to_string(),
                        }));
                    }
                    // thinking 暂时不转发——Responses 的 reasoning item 需要 server 给的 id，
                    // 我们没保留；丢掉对 stateless 调用没影响。
                    _ => {}
                }
            }
            if !pending_text.is_empty() {
                out.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": pending_text,
                }));
            }
        }
    }
    out
}

fn tool_result_content_to_text(content: &[ToolResultContent]) -> String {
    let mut buf = String::new();
    for c in content {
        match c {
            ToolResultContent::Text { text } => buf.push_str(text),
            ToolResultContent::Image { mime, .. } => {
                buf.push_str(&format!("[image/{mime} omitted]"))
            }
            ToolResultContent::Json { raw } => buf.push_str(&raw.to_string()),
        }
        buf.push('\n');
    }
    buf.trim_end().to_string()
}

// ===== SSE 解码器 =========================================================

struct ResponsesDecoder {
    /// 按 output_index 索引的累积 item。Responses API 严格要求 output_index 单调递增。
    items: Vec<DecoderItem>,
    /// 最终 stop_reason（来自 response.completed.payload.response.status / incomplete_details）
    stop_reason: Option<StopReason>,
    /// 最后一次拿到的 usage。
    usage: Option<TokenUsage>,
    /// response.completed 时已经组装好的 final——usage 先 emit，final 下一次 emit。
    pending_final: Option<ProviderEvent>,
    completed: bool,
}

#[derive(Debug, Clone)]
enum DecoderItem {
    /// assistant 文本 message item——按 content_part 累积
    Message { text: String },
    /// function_call item——arguments 流式拼接
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// reasoning item（gpt-5 系列）——v1 不转发，仅占位防止后续 item 索引错位
    Reasoning,
    /// 其他类型（web_search_call 等内置工具）——v1 不在 final message 里展开
    Other,
}

impl ResponsesDecoder {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            stop_reason: None,
            usage: None,
            pending_final: None,
            completed: false,
        }
    }

    fn consume(&mut self, event: &str, data: &str) -> Result<Option<ProviderEvent>, ProviderError> {
        if data.is_empty() {
            return Ok(None);
        }
        let v: Value = serde_json::from_str(data).map_err(|err| {
            ProviderError::Protocol(format!(
                "openai responses data 不是合法 JSON ({event}): {err}"
            ))
        })?;

        match event {
            // 一个 output item 开始——根据 type 创建占位
            "response.output_item.added" => {
                let index = v.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let item = v
                    .get("item")
                    .ok_or_else(|| ProviderError::Protocol("缺 item".into()))?;
                let item_type = item
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ProviderError::Protocol("item 缺 type".into()))?;
                let new_item = match item_type {
                    "message" => DecoderItem::Message {
                        text: String::new(),
                    },
                    "function_call" => DecoderItem::FunctionCall {
                        call_id: item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        name: item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        arguments: item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    },
                    "reasoning" => DecoderItem::Reasoning,
                    _ => DecoderItem::Other,
                };
                while self.items.len() <= index {
                    self.items.push(DecoderItem::Other);
                }
                self.items[index] = new_item;
                Ok(None)
            }
            "response.output_text.delta" => {
                let index = v.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = v.get("delta").and_then(Value::as_str).unwrap_or("");
                if let Some(DecoderItem::Message { text }) = self.items.get_mut(index) {
                    text.push_str(delta);
                }
                if delta.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(ProviderEvent::TextDelta(delta.to_string())))
                }
            }
            "response.function_call_arguments.delta" => {
                let index = v.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = v.get("delta").and_then(Value::as_str).unwrap_or("");
                if let Some(DecoderItem::FunctionCall { arguments, .. }) = self.items.get_mut(index)
                {
                    arguments.push_str(delta);
                }
                Ok(None)
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                // gpt-5 系列在配置里要求 summary 输出时会发——透传给 UI 当 thinking
                let delta = v.get("delta").and_then(Value::as_str).unwrap_or("");
                if delta.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(ProviderEvent::ThinkingDelta(delta.to_string())))
                }
            }
            "response.output_item.done" => {
                // item 收尾——如果是 function_call，从这里拿 final arguments（更可靠）
                let index = v.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(item) = v.get("item") {
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        if let Some(DecoderItem::FunctionCall {
                            call_id,
                            name,
                            arguments,
                        }) = self.items.get_mut(index)
                        {
                            if let Some(s) = item.get("call_id").and_then(Value::as_str) {
                                if !s.is_empty() {
                                    *call_id = s.to_string();
                                }
                            }
                            if let Some(s) = item.get("name").and_then(Value::as_str) {
                                if !s.is_empty() {
                                    *name = s.to_string();
                                }
                            }
                            // 用 done 给的最终 arguments（防 delta 累积漏字符）
                            if let Some(s) = item.get("arguments").and_then(Value::as_str) {
                                if !s.is_empty() {
                                    *arguments = s.to_string();
                                }
                            }
                        }
                    }
                }
                Ok(None)
            }
            "response.completed" => {
                self.completed = true;
                if let Some(resp) = v.get("response") {
                    if let Some(u) = resp.get("usage") {
                        self.usage = Some(parse_usage(u));
                    }
                    if let Some(reason) = resp
                        .get("incomplete_details")
                        .and_then(|d| d.get("reason"))
                        .and_then(Value::as_str)
                    {
                        self.stop_reason = Some(parse_stop_reason(reason));
                    } else if resp.get("status").and_then(Value::as_str) == Some("completed") {
                        self.stop_reason = Some(StopReason::EndTurn);
                    }
                }
                let usage_event = self.usage.clone().map(ProviderEvent::Usage);
                let final_event = self.assemble_final()?;
                // 一次只能 emit 一个 ProviderEvent；usage 优先，final 在下一次 yield。
                // 不过我们的流是 stream::unfold——不能一次 yield 两个。把 final 丢到
                // pending_final 字段不太干净，简单办法是把 usage merge 进 final 后只 emit final。
                // 反正 loop 在 MessageComplete 之前看 usage 就够了——把 usage 单独 emit
                // 一遍（相当于"在 message_stop 前"），再返回 final。
                if let Some(u) = usage_event {
                    // 把 final 暂存到 pending，下一次 consume 时给（但下一次只有 [DONE] 或别的）。
                    // 简单做法：直接 emit usage，final 留到下次 SSE 事件触发。但 response.completed
                    // 通常就是最后一条，下次进 None 分支，会报"流提前结束"。
                    //
                    // 改成：把 final 塞回 stop_reason 字段里"延后 emit"——但 unfold 状态机
                    // 看不到。最干净的：把 ResponsesDecoder.completed 的语义稍微调整——
                    // 收到 response.completed 时先 emit usage，把 final 暂存到 pending；
                    // 下一次 consume 任意事件先 drain pending。
                    //
                    // 这里就用 pending_final 字段：
                    self.pending_final = Some(final_event);
                    return Ok(Some(u));
                }
                Ok(Some(final_event))
            }
            "response.failed" | "response.error" => {
                let msg = v
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .or_else(|| v.get("message").and_then(Value::as_str))
                    .unwrap_or("openai responses error");
                Err(ProviderError::Request {
                    status: 0,
                    body: msg.to_string(),
                })
            }
            _ => {
                // 其他事件（response.created / response.in_progress / response.content_part.* /
                // response.web_search_call.* 等）我们不处理。但注意：如果 pending_final 已经
                // 暂存（response.completed 之后又来事件），需要 flush。
                if let Some(final_ev) = self.pending_final.take() {
                    return Ok(Some(final_ev));
                }
                Ok(None)
            }
        }
    }

    fn assemble_final(&mut self) -> Result<ProviderEvent, ProviderError> {
        let items = std::mem::take(&mut self.items);
        let mut content: Vec<Block> = Vec::new();
        for item in items {
            match item {
                DecoderItem::Message { text } => {
                    if !text.is_empty() {
                        content.push(Block::Text {
                            text,
                            cache_control: false,
                        });
                    }
                }
                DecoderItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                } => {
                    let input: Value = if arguments.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&arguments).map_err(|err| {
                            ProviderError::Protocol(format!(
                                "function_call arguments JSON 解析失败 (name={name}, raw={arguments}): {err}"
                            ))
                        })?
                    };
                    content.push(Block::ToolUse {
                        id: call_id,
                        name,
                        input,
                        server_side: false,
                    });
                }
                DecoderItem::Reasoning | DecoderItem::Other => {}
            }
        }
        let stop_reason = self.stop_reason.unwrap_or(StopReason::EndTurn);
        Ok(ProviderEvent::MessageComplete {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason,
        })
    }
}

fn parse_stop_reason(s: &str) -> StopReason {
    match s {
        "max_output_tokens" | "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "content_filter" | "refusal" => StopReason::Refusal,
        _ => StopReason::EndTurn,
    }
}

fn parse_usage(v: &Value) -> TokenUsage {
    let cached = v
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    TokenUsage {
        input_tokens: v.get("input_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
        output_tokens: v.get("output_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
        cache_read_tokens: cached,
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
            tools: vec![ToolDef::Local {
                name: "get_quote".into(),
                description: "拉行情".into(),
                input_schema: json!({"type": "object"}),
                cache_control: false,
            }],
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
                temperature: Some(0.3),
                top_p: None,
                thinking: None,
                effort: None,
                max_turns: 10,
                stop_sequences: vec![],
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
    fn build_request_body_uses_max_output_tokens_and_instructions() {
        let provider = OpenAIResponsesProvider::new(OpenAIResponsesConfig::new(
            "https://api.openai.com",
            "sk-test",
        ))
        .unwrap();
        let body = provider.build_request_body(&dummy_request());
        // Responses API 用 max_output_tokens（不是 max_tokens）
        assert_eq!(body["max_output_tokens"], 1024);
        assert!(body.get("max_tokens").is_none());
        // system → instructions
        assert_eq!(body["instructions"], "you are a stock assistant");
        // input 是 typed items 数组
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        // tools 内部 tagged
        let tool = &body["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["name"], "get_quote");
        assert_eq!(tool["strict"], false);
    }

    #[test]
    fn reasoning_effort_serialized_when_set() {
        let mut cfg = OpenAIResponsesConfig::new("https://api.openai.com", "sk-test");
        cfg.reasoning_effort = Some(ReasoningEffort::High);
        let provider = OpenAIResponsesProvider::new(cfg).unwrap();
        let body = provider.build_request_body(&dummy_request());
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn web_search_added_when_enabled() {
        let mut cfg = OpenAIResponsesConfig::new("https://api.openai.com", "sk-test");
        cfg.enable_web_search = true;
        let provider = OpenAIResponsesProvider::new(cfg).unwrap();
        let body = provider.build_request_body(&dummy_request());
        let tools = body["tools"].as_array().unwrap();
        assert!(tools.iter().any(|t| t["type"] == "web_search"));
    }

    #[test]
    fn tool_use_becomes_function_call_item() {
        let provider = OpenAIResponsesProvider::new(OpenAIResponsesConfig::new(
            "https://api.openai.com",
            "sk-test",
        ))
        .unwrap();
        let mut req = dummy_request();
        req.messages.push(Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "fc_42".into(),
                name: "get_quote".into(),
                input: json!({"code": "600519"}),
                server_side: false,
            }],
        });
        let body = provider.build_request_body(&req);
        let input = body["input"].as_array().unwrap();
        let fc = input.iter().find(|i| i["type"] == "function_call").unwrap();
        assert_eq!(fc["call_id"], "fc_42");
        assert_eq!(fc["name"], "get_quote");
        // arguments 必须是 string，不是 object
        let args = fc["arguments"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(args).unwrap();
        assert_eq!(parsed["code"], "600519");
    }

    #[test]
    fn tool_result_becomes_function_call_output_item() {
        let provider = OpenAIResponsesProvider::new(OpenAIResponsesConfig::new(
            "https://api.openai.com",
            "sk-test",
        ))
        .unwrap();
        let mut req = dummy_request();
        req.messages.push(Message {
            role: Role::User,
            content: vec![Block::ToolResult {
                tool_use_id: "fc_42".into(),
                content: vec![ToolResultContent::Text {
                    text: "1888".into(),
                }],
                is_error: false,
                server_side: false,
                cache_control: false,
            }],
        });
        let body = provider.build_request_body(&req);
        let input = body["input"].as_array().unwrap();
        let fco = input
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .unwrap();
        assert_eq!(fco["call_id"], "fc_42");
        assert_eq!(fco["output"], "1888");
    }

    #[test]
    fn assistant_items_preserve_text_then_tool_order() {
        // canonical: assistant blocks 是 [Text("我查一下"), ToolUse(get_quote)]。
        // wire 上 Items 应该是 [message(text), function_call]，不是 [function_call, message]。
        let provider = OpenAIResponsesProvider::new(OpenAIResponsesConfig::new(
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
                    id: "fc_1".into(),
                    name: "get_quote".into(),
                    input: json!({"code": "600519"}),
                    server_side: false,
                },
            ],
        });
        let body = provider.build_request_body(&req);
        let input = body["input"].as_array().unwrap();
        // 找到 [Text, ToolUse] 在 input 数组里的位置
        let msg_idx = input
            .iter()
            .position(|i| i["type"] == "message" && i["role"] == "assistant")
            .expect("应该有 assistant message item");
        let fc_idx = input
            .iter()
            .position(|i| i["type"] == "function_call")
            .expect("应该有 function_call item");
        assert!(
            msg_idx < fc_idx,
            "assistant message 必须出现在 function_call 之前（顺序 = canonical Block 顺序）"
        );
    }

    #[test]
    fn assistant_items_split_when_tool_between_texts() {
        // canonical: [Text("a"), ToolUse, Text("b")] 应该映射到
        // [message(a), function_call, message(b)] 三条 Items
        let provider = OpenAIResponsesProvider::new(OpenAIResponsesConfig::new(
            "https://api.openai.com",
            "sk-test",
        ))
        .unwrap();
        let mut req = dummy_request();
        req.messages.push(Message {
            role: Role::Assistant,
            content: vec![
                Block::Text {
                    text: "a".into(),
                    cache_control: false,
                },
                Block::ToolUse {
                    id: "fc_1".into(),
                    name: "get_quote".into(),
                    input: json!({}),
                    server_side: false,
                },
                Block::Text {
                    text: "b".into(),
                    cache_control: false,
                },
            ],
        });
        let body = provider.build_request_body(&req);
        let input = body["input"].as_array().unwrap();
        // 过滤掉初始 user message——只看追加的 assistant 三件套
        let assistant_items: Vec<&Value> = input
            .iter()
            .filter(|i| {
                i["type"] == "message" && i["role"] == "assistant" || i["type"] == "function_call"
            })
            .collect();
        assert_eq!(
            assistant_items.len(),
            3,
            "应该是 message + function_call + message 三条"
        );
        assert_eq!(assistant_items[0]["type"], "message");
        assert_eq!(assistant_items[0]["content"][0]["text"], "a");
        assert_eq!(assistant_items[1]["type"], "function_call");
        assert_eq!(assistant_items[2]["type"], "message");
        assert_eq!(assistant_items[2]["content"][0]["text"], "b");
    }

    #[test]
    fn rejects_empty_token() {
        let err =
            OpenAIResponsesProvider::new(OpenAIResponsesConfig::new("https://x", "")).unwrap_err();
        assert!(matches!(err, ProviderError::Config(_)));
    }

    // ===== SSE 解码 =====

    #[test]
    fn responses_decoder_assembles_text_message() {
        let mut dec = ResponsesDecoder::new();
        // output_item.added: message
        let _ = dec
            .consume(
                "response.output_item.added",
                r#"{"output_index":0,"item":{"type":"message","role":"assistant"}}"#,
            )
            .unwrap();
        // output_text.delta
        let ev = dec
            .consume(
                "response.output_text.delta",
                r#"{"output_index":0,"delta":"hi"}"#,
            )
            .unwrap();
        match ev {
            Some(ProviderEvent::TextDelta(s)) => assert_eq!(s, "hi"),
            _ => panic!(),
        }
        // response.completed → 先 emit Usage（如果有），final 暂存到 pending
        let usage_ev = dec
            .consume(
                "response.completed",
                r#"{"response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":1,"input_tokens_details":{"cached_tokens":3}}}}"#,
            )
            .unwrap();
        match usage_ev {
            Some(ProviderEvent::Usage(u)) => {
                assert_eq!(u.input_tokens, 10);
                assert_eq!(u.cache_read_tokens, 3);
            }
            _ => panic!(),
        }
        // pending final 应该已经设好了——unfold 在 None 分支会拿
        assert!(dec.pending_final.is_some());
        let final_ev = dec.pending_final.take().unwrap();
        match final_ev {
            ProviderEvent::MessageComplete {
                message,
                stop_reason,
            } => {
                assert_eq!(stop_reason, StopReason::EndTurn);
                match &message.content[0] {
                    Block::Text { text, .. } => assert_eq!(text, "hi"),
                    _ => panic!(),
                }
            }
            _ => panic!(),
        }
    }

    #[test]
    fn responses_decoder_assembles_function_call() {
        let mut dec = ResponsesDecoder::new();
        let _ = dec.consume(
            "response.output_item.added",
            r#"{"output_index":0,"item":{"type":"function_call","call_id":"fc_1","name":"get_quote","arguments":""}}"#,
        );
        let _ = dec.consume(
            "response.function_call_arguments.delta",
            r#"{"output_index":0,"delta":"{\"co"}"#,
        );
        let _ = dec.consume(
            "response.function_call_arguments.delta",
            r#"{"output_index":0,"delta":"de\":\"600519\"}"}"#,
        );
        let _ = dec.consume(
            "response.output_item.done",
            r#"{"output_index":0,"item":{"type":"function_call","call_id":"fc_1","name":"get_quote","arguments":"{\"code\":\"600519\"}"}}"#,
        );
        let _ = dec
            .consume(
                "response.completed",
                r#"{"response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            )
            .unwrap();
        let final_ev = dec.pending_final.take().unwrap();
        match final_ev {
            ProviderEvent::MessageComplete { message, .. } => {
                let tu = &message.content[0];
                match tu {
                    Block::ToolUse {
                        id, name, input, ..
                    } => {
                        assert_eq!(id, "fc_1");
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
    fn responses_decoder_maps_max_output_tokens_to_max_tokens() {
        let mut dec = ResponsesDecoder::new();
        let _ = dec.consume(
            "response.output_item.added",
            r#"{"output_index":0,"item":{"type":"message","role":"assistant"}}"#,
        );
        let _ = dec.consume(
            "response.output_text.delta",
            r#"{"output_index":0,"delta":"halfway"}"#,
        );
        let _ = dec.consume(
            "response.completed",
            r#"{"response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":1,"output_tokens":1}}}"#,
        );
        let final_ev = dec.pending_final.take().unwrap();
        match final_ev {
            ProviderEvent::MessageComplete { stop_reason, .. } => {
                assert_eq!(stop_reason, StopReason::MaxTokens);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn responses_decoder_returns_error_on_failed_event() {
        let mut dec = ResponsesDecoder::new();
        let result = dec.consume(
            "response.failed",
            r#"{"error":{"message":"server overloaded"}}"#,
        );
        match result {
            Err(ProviderError::Request { body, .. }) => {
                assert!(body.contains("server overloaded"))
            }
            _ => panic!(),
        }
    }
}
