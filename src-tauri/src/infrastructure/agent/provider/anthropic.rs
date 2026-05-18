//! AnthropicProvider — 直连 `{base_url}/v1/messages` 的 SSE 实现。
//!
//! 协议要点（Anthropic Messages API 2023-06-01）：
//! - 请求 body：system 是数组（每个元素 {type:"text", text, cache_control?}）；
//!   tools 是数组（{name, description, input_schema, cache_control?} 或
//!   {type:"web_search_20250305", ...}）；messages 是数组（{role, content:[block]}）。
//! - cache_control 是 `{"type": "ephemeral"}` 对象——我们的 canonical 用 bool，
//!   翻译时拼成对象。整次请求最多 4 个 breakpoint。
//! - SSE 事件：message_start / content_block_start / content_block_delta /
//!   content_block_stop / message_delta / message_stop / ping / error。
//! - tool_use 的 input 是流式拼接的（input_json_delta 给 partial_json 字符串），
//!   要等 content_block_stop 收齐后再 parse 成 JSON。
//! - server_tool_use / web_search_tool_result 是 provider 替我们执行 web_search
//!   的产物——loop 不要执行，原样转回下一轮即可。

use crate::domain::agent::types::{
    AgentRequest, Block, Message, Role, ServerSideTool, StopReason, SystemBlock, ThinkingConfig,
    ThinkingDisplay, ToolDef, ToolResultContent,
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

pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// 不带尾斜杠的 base，例如 `https://api.anthropic.com` 或 `https://coding.xxx`。
    pub base_url: String,
    /// `cr_xxx` 或官方 `sk-ant-xxx`。走 `x-api-key` header。
    pub token: String,
    /// HTTP 调用超时。SSE 长流要给足时间——briefing 可能跑几十秒。
    pub request_timeout: Duration,
}

impl AnthropicConfig {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            request_timeout: Duration::from_secs(300),
        }
    }
}

#[derive(Debug)]
pub struct AnthropicProvider {
    config: AnthropicConfig,
    http: Client,
}

impl AnthropicProvider {
    pub fn new(config: AnthropicConfig) -> Result<Self, ProviderError> {
        if config.token.trim().is_empty() {
            return Err(ProviderError::Config("anthropic token 为空".into()));
        }
        if !config.base_url.starts_with("http") {
            return Err(ProviderError::Config(format!(
                "anthropic base_url 不合法：{}",
                config.base_url
            )));
        }
        // 强制 HTTP/1.1：reqwest 默认走 HTTP/2 协商，但部分 Cloudflare-fronted
        // claude-relay 在长 SSE 流上 HTTP/2 行为异常（实测 curl --http1.1/--http2
        // 都收到完整 message_stop，但 reqwest 默认走 HTTP/2 时偶发 ~3s 后流被对端
        // 关闭、收不到 message_stop）。HTTP/1.1 keep-alive + Transfer-Encoding: chunked
        // 是 SSE 最广泛兼容的形态——把它锁死避免协商成 HTTP/2 撞这类 relay 的坑。
        let http = Client::builder()
            .timeout(config.request_timeout)
            .pool_idle_timeout(Some(Duration::from_secs(90)))
            .http1_only()
            .build()
            .map_err(|err| ProviderError::Config(format!("构建 http client 失败：{err}")))?;
        Ok(Self { config, http })
    }

    fn build_request_body(&self, req: &AgentRequest) -> Value {
        let mut body = Map::new();
        body.insert("model".into(), json!(req.options.model));
        body.insert("max_tokens".into(), json!(req.options.max_tokens));
        body.insert("stream".into(), json!(true));

        // temperature 在两类模型上**会被 API 拒收**：
        //   1. 启用 thinking 的请求（thinking 自带温度策略）
        //   2. opus-4-7 / 后续 4.X 推理优先模型（API 报 "`temperature` is deprecated
        //      for this model."），不论 thinking 字段是否启用
        // 把这两个条件合并——pipeline 端给的温度作"用户期望"对待，wire format 层决定能否实发。
        let model_supports_temperature = !is_temperature_deprecated_model(&req.options.model);
        if req.options.thinking.is_none() && model_supports_temperature {
            if let Some(t) = req.options.temperature {
                body.insert("temperature".into(), json!(t));
            }
        }
        if let Some(p) = req.options.top_p {
            body.insert("top_p".into(), json!(p));
        }
        if !req.options.stop_sequences.is_empty() {
            body.insert("stop_sequences".into(), json!(req.options.stop_sequences));
        }

        // thinking——按模型做兼容性 normalize（详见 normalize_thinking_for_model）。
        if let Some(t) = req.options.thinking.as_ref() {
            if let Some(wire) = normalize_thinking_for_model(&req.options.model, t) {
                body.insert("thinking".into(), wire);
            }
        }

        // output_config.effort——4.6+ Anthropic 模型识别。本字段独立于 thinking，
        // 也会影响 tool call 数量 / text response 长度。
        if let Some(effort) = req.options.effort {
            body.insert("output_config".into(), json!({"effort": effort.as_str()}));
        }

        if !req.system.is_empty() {
            body.insert(
                "system".into(),
                Value::Array(req.system.iter().map(system_block_to_wire).collect()),
            );
        }
        if !req.tools.is_empty() {
            let model = req.options.model.as_str();
            body.insert(
                "tools".into(),
                Value::Array(
                    req.tools
                        .iter()
                        .map(|t| tool_def_to_wire(t, model))
                        .collect(),
                ),
            );
        }
        body.insert(
            "messages".into(),
            Value::Array(req.messages.iter().map(message_to_wire).collect()),
        );

        Value::Object(body)
    }
}

#[async_trait]
impl ChatProvider for AnthropicProvider {
    async fn stream(
        &self,
        req: &AgentRequest,
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError> {
        let body = self.build_request_body(req);
        // 调试 SSE 提前断的诊断手段：env GANGZI_DUMP_REQ=1 时每次请求把
        // body 序列化到 /tmp/anthropic-last-req.json，用户可以贴给我或自己 curl
        // 复现关闭。生产无影响——env 没设就跳过。
        if std::env::var("GANGZI_DUMP_REQ").as_deref() == Ok("1") {
            if let Ok(s) = serde_json::to_string_pretty(&body) {
                let _ = std::fs::write("/tmp/anthropic-last-req.json", s);
                tracing::info!(
                    bytes = body.to_string().len(),
                    "已转储 anthropic 请求体到 /tmp/anthropic-last-req.json"
                );
            }
        }
        let url = format!("{}/v1/messages", self.config.base_url);
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.config.token)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|err| {
                if err.is_timeout() || err.is_connect() {
                    ProviderError::Transient(format!("anthropic 网络错误：{err}"))
                } else {
                    ProviderError::Transient(format!("anthropic 请求失败：{err}"))
                }
            })?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(map_http_error(status.as_u16(), body_text));
        }

        // reqwest::Response 的 bytes_stream 给我们 Stream<Result<Bytes, reqwest::Error>>，
        // eventsource 适配器把它变成 Stream<Result<Event, EventStreamError>>。
        let event_stream = resp.bytes_stream().eventsource();

        // 在 stream 上挂一个有状态的 SSE 解码器，把 Anthropic 的若干 SSE 事件
        // 翻译成我们的 ProviderEvent。fold + state machine 比 map 干净。
        let translated = stream::unfold(
            (event_stream, SseDecoder::new()),
            |(mut es, mut decoder)| async move {
                loop {
                    match es.next().await {
                        None => {
                            // 流被远端关闭。按 Anthropic 官方流式协议，**message_stop
                            // 是必发收尾事件**——拿不到说明 provider/relay 在协议层
                            // 出问题（连接被 reset、proxy 异常截断、上游 5xx 等）。
                            // 不做软兜底——把错误暴露出来，由调用方决定 retry 还是
                            // 报警。掩盖会让"半截响应"被当成完整结果落库，更糟。
                            if !decoder.completed {
                                return Some((
                                    Err(ProviderError::Protocol(
                                        "SSE 流提前结束，没有 message_stop".into(),
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
                            Ok(None) => continue, // 累积中，未到可 emit 的事件，吸下一条
                            Err(err) => return Some((Err(err), (es, decoder))),
                        },
                    }
                }
            },
        );

        Ok(translated.boxed())
    }
}

// ===== Wire-format 翻译（canonical → Anthropic JSON）======================

/// 解码 SSE error 事件 payload 成 [`ProviderError`]。
///
/// 三种已知形态：
///   A. `{"error": {"type": "...", "message": "..."}}` —— Anthropic 官方
///   B. `{"error": "<pydantic validation msg>"}` —— claude-relay 校验失败
///   C. `{"error": "Claude API error", "status": 404, "details": "<json>"}`
///      —— claude-relay 把上游 4xx 响应整体包装
///
/// 出参规则：
/// - 已知 type / status → 按 HTTP 语义分类（4xx 不重试，5xx / 限流重试）
/// - 未知形态 → 保守按 Transient（让 retry 试一次）
/// - body 必须包含**真实错误文本**（哪个字段炸 / 什么模型不存在），UI 才能定位
fn decode_error_event(v: &Value) -> ProviderError {
    let err_field = v.get("error");
    match err_field {
        // A: 官方 object 形态
        Some(Value::Object(_)) => {
            let err_type = err_field
                .and_then(|e| e.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let msg = err_field
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("anthropic error event");
            let body = format!("{err_type}: {msg}");
            match err_type {
                "overloaded_error" | "rate_limit_error" => ProviderError::RateLimited(body),
                "api_error" => ProviderError::Transient(body),
                "invalid_request_error" => ProviderError::Request { status: 400, body },
                "authentication_error" => ProviderError::Request { status: 401, body },
                "permission_error" => ProviderError::Request { status: 403, body },
                "not_found_error" => ProviderError::Request { status: 404, body },
                "request_too_large" => ProviderError::Request { status: 413, body },
                "billing_error" => ProviderError::Request { status: 402, body },
                _ => ProviderError::Transient(body),
            }
        }
        Some(Value::String(s)) => {
            // 形态 B vs C 通过顶层有没有 `status` / `details` 区分。
            let status = v.get("status").and_then(Value::as_u64).map(|n| n as u16);
            let details_inner = v.get("details").and_then(Value::as_str);

            // 优先从 details（stringified JSON）里挖原始 error.type/message——relay 包装
            // 形态里这块通常是 Anthropic 原生 4xx body 的副本，最有定位价值。
            let inner_parsed: Option<Value> =
                details_inner.and_then(|s| serde_json::from_str(s).ok());
            let inner_err = inner_parsed.as_ref().and_then(|j| j.get("error"));
            let inner_type = inner_err
                .and_then(|e| e.get("type"))
                .and_then(Value::as_str);
            let inner_msg = inner_err
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str);

            let body = match (inner_type, inner_msg) {
                (Some(t), Some(m)) => format!("{s} (status={status:?}, {t}: {m})"),
                _ => match status {
                    Some(st) => format!("{s} (status={st})"),
                    None => s.clone(),
                },
            };

            // 状态分类——优先 status，其次 inner_type
            if let Some(st) = status {
                if st == 429 || st == 529 {
                    return ProviderError::RateLimited(body);
                }
                if (500..600).contains(&st) {
                    return ProviderError::Transient(body);
                }
                if (400..500).contains(&st) {
                    return ProviderError::Request { status: st, body };
                }
            }
            if let Some(t) = inner_type {
                return match t {
                    "overloaded_error" | "rate_limit_error" => ProviderError::RateLimited(body),
                    "api_error" => ProviderError::Transient(body),
                    "not_found_error" => ProviderError::Request { status: 404, body },
                    "invalid_request_error" => ProviderError::Request { status: 400, body },
                    "authentication_error" => ProviderError::Request { status: 401, body },
                    "permission_error" => ProviderError::Request { status: 403, body },
                    _ => ProviderError::Request { status: 400, body },
                };
            }
            // 没有状态信息——视作 400 校验错（不重试），但把原始字符串暴露
            ProviderError::Request { status: 400, body }
        }
        _ => ProviderError::Transient("anthropic error event 缺 error 字段".into()),
    }
}

/// 判断模型是否拒收 `temperature` 参数。
///
/// 当前命中：opus-4 / opus-5 全系（实测 SSE error: "`temperature` is deprecated
/// for this model."）。匹配错了顶多丢失 0.3 vs 1.0 的微调效果，匹配漏了会让 briefing
/// 全线挂掉，后者代价大得多。
fn is_temperature_deprecated_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("opus-4") || m.contains("opus-5")
}

/// 判断模型是否完全不支持 thinking（任何模式）。
///
/// Haiku 系列（4.5 及更早）不在 adaptive thinking 的 supported models 列表里，
/// 而对老 Haiku 我们也从来不开 thinking——统一 drop 字段最稳。
fn is_thinking_unsupported_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("haiku")
}

/// 判断模型是否拒收 manual extended thinking（`type: enabled, budget_tokens`）。
///
/// Opus 4.7 起 manual 模式直接返回 400（文档明确："no longer supported on
/// Claude Opus 4.7 and returns a 400 error"）。Opus 5+ 跟进保守按此规则。
/// 这些模型需要 adaptive 模式。
fn is_manual_thinking_rejected_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("opus-4-7") || m.contains("opus-5")
}

/// 把 canonical [`ThinkingConfig`] 翻译成 Anthropic wire format JSON。
///
/// 同时按模型做必要的 normalize：
/// - Haiku 系列：完全不支持 thinking，返回 `None` 让 caller drop 整个字段
/// - Opus 4.7 + manual：自动转 adaptive（manual 会撞 400）
/// - Opus 4.7 + adaptive 默认 display：force `summarized`，否则 UI 看不到思考流
///   （Opus 4.7 API 默认 `omitted`）
fn normalize_thinking_for_model(model: &str, cfg: &ThinkingConfig) -> Option<Value> {
    if is_thinking_unsupported_model(model) {
        tracing::warn!(model, "模型不支持 thinking，已 drop 字段");
        return None;
    }

    let mut obj = Map::new();
    match cfg {
        ThinkingConfig::Adaptive { display } => {
            obj.insert("type".into(), json!("adaptive"));
            // Opus 4.7 默认 display=omitted（API 行为变更）——我们前端要渲染思考流，
            // 显式设 summarized 让用户能看到。其他模型默认 summarized，显式标也无害。
            let resolved = display.unwrap_or(ThinkingDisplay::Summarized);
            obj.insert("display".into(), json!(resolved.as_str()));
        }
        ThinkingConfig::Enabled { budget_tokens } => {
            if is_manual_thinking_rejected_model(model) {
                tracing::warn!(
                    model,
                    "模型不接受 manual thinking，自动转 adaptive（summarized）"
                );
                obj.insert("type".into(), json!("adaptive"));
                obj.insert("display".into(), json!("summarized"));
            } else {
                obj.insert("type".into(), json!("enabled"));
                obj.insert("budget_tokens".into(), json!(budget_tokens));
            }
        }
    }
    Some(Value::Object(obj))
}

fn system_block_to_wire(block: &SystemBlock) -> Value {
    let mut obj = json!({"type": "text", "text": block.text});
    if block.cache_control {
        obj["cache_control"] = json!({"type": "ephemeral"});
    }
    obj
}

fn tool_def_to_wire(tool: &ToolDef, model: &str) -> Value {
    match tool {
        ToolDef::Local {
            name,
            description,
            input_schema,
            cache_control,
        } => {
            let mut obj = json!({
                "name": name,
                "description": description,
                "input_schema": input_schema,
            });
            if *cache_control {
                obj["cache_control"] = json!({"type": "ephemeral"});
            }
            obj
        }
        ToolDef::ServerSide(ServerSideTool::AnthropicWebSearch {
            name,
            max_uses,
            allowed_domains,
            blocked_domains,
        }) => {
            let mut obj = json!({
                "type": web_search_tool_version_for_model(model),
                "name": name,
            });
            if let Some(n) = max_uses {
                obj["max_uses"] = json!(n);
            }
            if !allowed_domains.is_empty() {
                obj["allowed_domains"] = json!(allowed_domains);
            }
            if !blocked_domains.is_empty() {
                obj["blocked_domains"] = json!(blocked_domains);
            }
            obj
        }
    }
}

/// 选择 web_search 工具版本——按模型代际分发。
///
/// 新版 `web_search_20260209` 在 Opus 4.7 / Opus 4.6 / Sonnet 4.6 / Mythos Preview
/// 上可用，提供 dynamic filtering 能力（需要同时启用 code_execution 工具才会生效）。
/// 我们当前不接 code_execution——升级到 20260209 = **拿到工具版本兼容性 + 行为
/// 退化为基本模式**（和 20250305 一样），无副作用收益。
///
/// 老模型（Sonnet 4.5、Opus 4.5、早期 4.x）维持 20250305——文档没说老模型支持新版
/// 字符串，保守起见不冒险。
fn web_search_tool_version_for_model(model: &str) -> &'static str {
    let m = model.to_ascii_lowercase();
    let supports_new = m.contains("opus-4-7")
        || m.contains("opus-4-6")
        || m.contains("sonnet-4-6")
        || m.contains("mythos");
    if supports_new {
        "web_search_20260209"
    } else {
        "web_search_20250305"
    }
}

fn message_to_wire(msg: &Message) -> Value {
    json!({
        "role": match msg.role { Role::User => "user", Role::Assistant => "assistant" },
        "content": Value::Array(msg.content.iter().map(block_to_wire).collect()),
    })
}

fn block_to_wire(block: &Block) -> Value {
    match block {
        Block::Text {
            text,
            cache_control,
        } => {
            let mut obj = json!({"type": "text", "text": text});
            if *cache_control {
                obj["cache_control"] = json!({"type": "ephemeral"});
            }
            obj
        }
        Block::Thinking {
            thinking,
            signature,
        } => {
            let mut obj = json!({"type": "thinking", "thinking": thinking});
            if let Some(sig) = signature {
                obj["signature"] = json!(sig);
            }
            obj
        }
        Block::RedactedThinking { data } => {
            json!({"type": "redacted_thinking", "data": data})
        }
        Block::Image { mime, data } => {
            json!({
                "type": "image",
                "source": {"type": "base64", "media_type": mime, "data": data}
            })
        }
        Block::ToolUse {
            id,
            name,
            input,
            server_side,
        } => {
            // 把模型上一回合产出的 server_tool_use 原样回传——Anthropic 要求保留
            let kind = if *server_side {
                "server_tool_use"
            } else {
                "tool_use"
            };
            json!({"type": kind, "id": id, "name": name, "input": input})
        }
        Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            server_side,
            cache_control,
        } => {
            // server_side 工具的结果用 web_search_tool_result（只支持 web_search）
            let kind = if *server_side {
                "web_search_tool_result"
            } else {
                "tool_result"
            };
            let content_value = tool_result_content_to_wire(content, *server_side);
            let mut obj = json!({
                "type": kind,
                "tool_use_id": tool_use_id,
                "content": content_value,
            });
            if *is_error {
                obj["is_error"] = json!(true);
            }
            if *cache_control {
                obj["cache_control"] = json!({"type": "ephemeral"});
            }
            obj
        }
    }
}

fn tool_result_content_to_wire(content: &[ToolResultContent], server_side: bool) -> Value {
    // server_side 的 web_search_tool_result.content 通常是一个原生数组，
    // 我们用 Json{raw} 透传，这里直接展开。
    if server_side {
        if let Some(ToolResultContent::Json { raw }) = content.first() {
            return raw.clone();
        }
    }
    Value::Array(
        content
            .iter()
            .map(|c| match c {
                ToolResultContent::Text { text } => json!({"type": "text", "text": text}),
                ToolResultContent::Image { mime, data } => json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": mime, "data": data}
                }),
                ToolResultContent::Json { raw } => raw.clone(),
            })
            .collect(),
    )
}

fn map_http_error(status: u16, body: String) -> ProviderError {
    match status {
        429 => ProviderError::RateLimited(body),
        500..=599 => ProviderError::Transient(format!("status={status} body={body}")),
        _ => ProviderError::Request { status, body },
    }
}

// ===== SSE 解码器 ========================================================

/// 有状态的 SSE → ProviderEvent 翻译器。
///
/// Anthropic 的 SSE 形态：每个 content block 由 content_block_start 开启，
/// 中间穿插任意条 content_block_delta，content_block_stop 收尾。
/// tool_use 的 input 是字符串拼接（partial_json）出来的——必须收齐 stop 后整块 parse。
///
/// 我们维护一个 blocks 数组（按 index 索引）+ 各种增量缓冲，message_stop
/// 触发时把所有 block 拼成一条 assistant Message 一起 emit。
struct SseDecoder {
    blocks: Vec<DecoderBlock>,
    stop_reason: Option<StopReason>,
    pending_usage: Option<TokenUsage>,
    completed: bool,
}

#[derive(Debug)]
enum DecoderBlock {
    Text {
        buf: String,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input_json_buf: String,
        server_side: bool,
    },
    /// web_search_tool_result block——内容已经是结构化的，直接 attach
    ServerToolResult {
        tool_use_id: String,
        content: Value,
    },
}

impl DecoderBlock {
    fn into_block(self) -> Result<Block, ProviderError> {
        Ok(match self {
            DecoderBlock::Text { buf } => Block::Text {
                text: buf,
                cache_control: false,
            },
            DecoderBlock::Thinking {
                thinking,
                signature,
            } => Block::Thinking {
                thinking,
                signature,
            },
            DecoderBlock::RedactedThinking { data } => Block::RedactedThinking { data },
            DecoderBlock::ToolUse {
                id,
                name,
                input_json_buf,
                server_side,
            } => {
                // 空字符串视作空对象——模型不传参数时 partial_json 全程没出现
                let input: Value = if input_json_buf.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&input_json_buf).map_err(|err| {
                        ProviderError::Protocol(format!(
                            "tool_use input JSON 解析失败 (name={name}, raw={input_json_buf}): {err}"
                        ))
                    })?
                };
                Block::ToolUse {
                    id,
                    name,
                    input,
                    server_side,
                }
            }
            DecoderBlock::ServerToolResult {
                tool_use_id,
                content,
            } => Block::ToolResult {
                tool_use_id,
                content: vec![ToolResultContent::Json { raw: content }],
                is_error: false,
                server_side: true,
                cache_control: false,
            },
        })
    }
}

impl SseDecoder {
    fn new() -> Self {
        Self {
            blocks: Vec::new(),
            stop_reason: None,
            pending_usage: None,
            completed: false,
        }
    }

    /// 把当前累积的 block 数组拼成 [`ProviderEvent::MessageComplete`]，
    /// 由 `message_stop` 调用。stop_reason 缺省回退到 `EndTurn`（下游 loop
    /// 看 message 内容是否含 tool_use 自己决定是否继续轮询）。
    fn finalize_message(&mut self) -> Result<ProviderEvent, ProviderError> {
        let blocks_drained = std::mem::take(&mut self.blocks);
        let mut content = Vec::with_capacity(blocks_drained.len());
        for b in blocks_drained {
            content.push(b.into_block()?);
        }
        let stop_reason = self.stop_reason.unwrap_or(StopReason::EndTurn);
        let message = Message {
            role: Role::Assistant,
            content,
        };
        self.completed = true;
        Ok(ProviderEvent::MessageComplete {
            message,
            stop_reason,
        })
    }

    /// 吸一条 SSE 事件，可能产出 0 或 1 条 ProviderEvent。
    fn consume(&mut self, event: &str, data: &str) -> Result<Option<ProviderEvent>, ProviderError> {
        // ping / 空事件——anthropic 偶尔发心跳保活
        if event == "ping" || data.is_empty() {
            return Ok(None);
        }
        let v: Value = serde_json::from_str(data).map_err(|err| {
            ProviderError::Protocol(format!("SSE data 不是合法 JSON ({event}): {err}"))
        })?;

        match event {
            "message_start" => {
                // 初始 usage 在这里给（含 cache_read / cache_creation）；后续
                // message_delta 会再给一条最终 usage 覆盖。
                if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                    self.pending_usage = Some(parse_usage(u));
                }
                Ok(None)
            }
            "content_block_start" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let block = v
                    .get("content_block")
                    .ok_or_else(|| ProviderError::Protocol("缺 content_block".into()))?;
                let block_type = block
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ProviderError::Protocol("content_block 缺 type".into()))?;
                let new_block = match block_type {
                    "text" => DecoderBlock::Text {
                        buf: block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    },
                    "thinking" => DecoderBlock::Thinking {
                        thinking: block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        signature: block
                            .get("signature")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    },
                    "redacted_thinking" => DecoderBlock::RedactedThinking {
                        data: block
                            .get("data")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    },
                    "tool_use" | "server_tool_use" => DecoderBlock::ToolUse {
                        id: block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        name: block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        input_json_buf: String::new(),
                        server_side: block_type == "server_tool_use",
                    },
                    "web_search_tool_result" => DecoderBlock::ServerToolResult {
                        tool_use_id: block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        content: block.get("content").cloned().unwrap_or(Value::Null),
                    },
                    other => {
                        return Err(ProviderError::Protocol(format!(
                            "未知 content_block type: {other}"
                        )))
                    }
                };
                if self.blocks.len() != index {
                    return Err(ProviderError::Protocol(format!(
                        "content_block_start index {index} 跳号，当前已收 {} 个 block",
                        self.blocks.len()
                    )));
                }
                self.blocks.push(new_block);
                Ok(None)
            }
            "content_block_delta" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = v
                    .get("delta")
                    .ok_or_else(|| ProviderError::Protocol("缺 delta".into()))?;
                let delta_type = delta
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ProviderError::Protocol("delta 缺 type".into()))?;
                let block = self.blocks.get_mut(index).ok_or_else(|| {
                    ProviderError::Protocol(format!("delta 引用了未开启的 block index={index}"))
                })?;
                match (delta_type, block) {
                    ("text_delta", DecoderBlock::Text { buf }) => {
                        let chunk = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        buf.push_str(chunk);
                        if !chunk.is_empty() {
                            return Ok(Some(ProviderEvent::TextDelta(chunk.to_string())));
                        }
                        Ok(None)
                    }
                    ("thinking_delta", DecoderBlock::Thinking { thinking, .. }) => {
                        let chunk = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                        thinking.push_str(chunk);
                        if !chunk.is_empty() {
                            return Ok(Some(ProviderEvent::ThinkingDelta(chunk.to_string())));
                        }
                        Ok(None)
                    }
                    ("signature_delta", DecoderBlock::Thinking { signature, .. }) => {
                        let chunk = delta.get("signature").and_then(Value::as_str).unwrap_or("");
                        let sig = signature.get_or_insert_with(String::new);
                        sig.push_str(chunk);
                        Ok(None)
                    }
                    ("input_json_delta", DecoderBlock::ToolUse { input_json_buf, .. }) => {
                        let chunk = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        input_json_buf.push_str(chunk);
                        Ok(None)
                    }
                    (other, blk) => Err(ProviderError::Protocol(format!(
                        "delta type {other} 与 block {:?} 不匹配",
                        std::mem::discriminant(blk)
                    ))),
                }
            }
            "content_block_stop" => Ok(None),
            "message_delta" => {
                if let Some(stop) = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(parse_stop_reason(stop));
                }
                if let Some(u) = v.get("usage") {
                    // message_delta.usage 是增量补充（一般含 output_tokens 最终值），
                    // 与 message_start.usage 合并。Anthropic 的 cache_read / cache_creation
                    // 字段在 message_start 里就给齐了，message_delta 通常只更新 output_tokens。
                    let mut combined = self.pending_usage.clone().unwrap_or_default();
                    let extra = parse_usage(u);
                    if extra.input_tokens > 0 {
                        combined.input_tokens = extra.input_tokens;
                    }
                    if extra.output_tokens > 0 {
                        combined.output_tokens = extra.output_tokens;
                    }
                    if extra.cache_read_tokens > 0 {
                        combined.cache_read_tokens = extra.cache_read_tokens;
                    }
                    if extra.cache_write_tokens > 0 {
                        combined.cache_write_tokens = extra.cache_write_tokens;
                    }
                    self.pending_usage = Some(combined.clone());
                    // 在 message_delta 这里把 Usage emit 出去——它发生在 message_stop 之前，
                    // 顺序天然正确（Usage 先 → MessageComplete 后），不需要额外队列。
                    return Ok(Some(ProviderEvent::Usage(combined)));
                }
                Ok(None)
            }
            "message_stop" => self.finalize_message().map(Some),
            "error" => {
                // Error 事件三种已知形态都要兜——必须把真实文本暴露给 UI，否则用户
                // 看到的就是泛泛的 "SSE 流提前结束"，无从下手。
                //
                //   A. Anthropic 官方形态：`{"error": {"type": "X", "message": "Y"}}`
                //   B. claude-relay 字符串形态：`{"error": "field: validation msg"}`
                //      —— pydantic 校验报错 dump 出来就是字符串
                //   C. claude-relay 包装形态：
                //      `{"error": "Claude API error", "status": 404,
                //        "details": "<stringified upstream JSON>", "timestamp": ...}`
                //      —— relay 把上游 4xx 响应整体包成自己的格式
                //
                // 参考：https://platform.claude.com/docs/en/api/errors
                let mapped = decode_error_event(&v);
                Err(mapped)
            }
            _ => Ok(None),
        }
    }
}

fn parse_stop_reason(s: &str) -> StopReason {
    match s {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::EndTurn, // 临时——loop 看 message 内容是否含 tool_use 决定是否继续
        "pause_turn" => StopReason::PauseTurn,
        "refusal" => StopReason::Refusal,
        _ => StopReason::EndTurn,
    }
}

fn parse_usage(v: &Value) -> TokenUsage {
    TokenUsage {
        input_tokens: v.get("input_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
        output_tokens: v.get("output_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
        cache_read_tokens: v
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        cache_write_tokens: v
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
    }
}

// ===== 单元测试 ==========================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::types::{
        AgentOptions, ContextBudget, PipelineKind, ServerSideTool, ThinkingConfig,
    };

    fn dummy_request() -> AgentRequest {
        AgentRequest {
            system: vec![SystemBlock {
                text: "你是 A 股助手".into(),
                cache_control: true,
            }],
            tools: vec![
                ToolDef::Local {
                    name: "get_quote".into(),
                    description: "拉行情".into(),
                    input_schema: json!({"type": "object"}),
                    cache_control: true,
                },
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
                model: "claude-sonnet-4-6".into(),
                max_tokens: 1024,
                temperature: Some(0.5),
                top_p: None,
                thinking: Some(ThinkingConfig::Enabled {
                    budget_tokens: 1000,
                }),
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
    fn build_request_body_shape() {
        let provider =
            AnthropicProvider::new(AnthropicConfig::new("https://api.example.com", "cr_test"))
                .unwrap();
        let body = provider.build_request_body(&dummy_request());
        assert_eq!(body["model"], "claude-sonnet-4-6");
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["stream"], true);
        // dummy_request 同时给了 temperature=0.5 和 thinking=enabled——Anthropic 要求
        // extended thinking 启用时不能带自定义 temperature，build_request_body 必须
        // 把 temperature 从 body 里抹掉。
        assert!(
            body.get("temperature").is_none(),
            "thinking 启用时 temperature 必须被抹掉"
        );
        assert!(body.get("top_p").is_none());
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 1000);

        // system 是数组，第一段带 cache_control
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");

        // tools：本地 + server-side 都序列化成对的形态。
        // sonnet-4-6 走 web_search_20260209 新版（支持模型列表里）。
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "get_quote");
        assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(tools[1]["type"], "web_search_20260209");
        assert_eq!(tools[1]["name"], "web_search");
        assert_eq!(tools[1]["max_uses"], 5);

        // messages：role + content blocks
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["type"], "text");
    }

    #[test]
    fn build_request_body_keeps_temperature_when_thinking_disabled() {
        let provider =
            AnthropicProvider::new(AnthropicConfig::new("https://api.example.com", "cr_test"))
                .unwrap();
        let mut req = dummy_request();
        req.options.thinking = None; // 关闭 thinking
        let body = provider.build_request_body(&req);
        assert_eq!(body["temperature"], 0.5);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn build_request_body_drops_temperature_for_opus_4_7() {
        // opus-4-7 等推理优先模型 deprecate 了 temperature——发出去会被 API 报错。
        // wire-format 层负责把 pipeline 给的温度静默丢掉，不污染请求。
        let provider =
            AnthropicProvider::new(AnthropicConfig::new("https://api.example.com", "cr_test"))
                .unwrap();
        let mut req = dummy_request();
        req.options.model = "claude-opus-4-7".into();
        req.options.thinking = None;
        req.options.temperature = Some(0.3);
        let body = provider.build_request_body(&req);
        assert!(
            body.get("temperature").is_none(),
            "opus-4-7 不应该带 temperature 字段，否则被 API 拒收"
        );
    }

    #[test]
    fn temperature_deprecated_helper_matches_opus_only() {
        assert!(is_temperature_deprecated_model("claude-opus-4-7"));
        assert!(is_temperature_deprecated_model("claude-opus-4-6"));
        assert!(is_temperature_deprecated_model("claude-opus-5"));
        assert!(!is_temperature_deprecated_model("claude-sonnet-4-6"));
        assert!(!is_temperature_deprecated_model("claude-haiku-4-5"));
    }

    // ===== normalize_thinking_for_model 模型护栏 =====

    use crate::domain::agent::types::ThinkingDisplay;

    #[test]
    fn thinking_haiku_drops_field_entirely() {
        // Haiku 不在 adaptive 支持列表里，也不需要 thinking——任何模式都 drop
        let adaptive = ThinkingConfig::Adaptive {
            display: Some(ThinkingDisplay::Summarized),
        };
        assert!(normalize_thinking_for_model("claude-haiku-4-5", &adaptive).is_none());

        let enabled = ThinkingConfig::Enabled {
            budget_tokens: 2000,
        };
        assert!(normalize_thinking_for_model("claude-haiku-4-5-20251001", &enabled).is_none());
    }

    #[test]
    fn thinking_opus_4_7_with_manual_auto_converts_to_adaptive() {
        // opus-4-7 manual thinking 撞 400——必须自动转 adaptive + 默认 summarized 让 UI 看到思考
        let enabled = ThinkingConfig::Enabled {
            budget_tokens: 5000,
        };
        let wire = normalize_thinking_for_model("claude-opus-4-7", &enabled).unwrap();
        assert_eq!(wire["type"], "adaptive");
        assert_eq!(wire["display"], "summarized");
        assert!(
            wire.get("budget_tokens").is_none(),
            "adaptive 模式不能带 budget_tokens"
        );
    }

    #[test]
    fn thinking_opus_4_7_adaptive_defaults_display_to_summarized() {
        // Opus 4.7 API 默认 display=omitted——前端要看到思考流必须显式 summarized
        let adaptive = ThinkingConfig::Adaptive { display: None };
        let wire = normalize_thinking_for_model("claude-opus-4-7", &adaptive).unwrap();
        assert_eq!(wire["type"], "adaptive");
        assert_eq!(wire["display"], "summarized");
    }

    #[test]
    fn thinking_sonnet_4_6_manual_preserved() {
        // Sonnet 4.6 仍接受 manual budget_tokens——deprecated 但功能在，原样下发
        let enabled = ThinkingConfig::Enabled {
            budget_tokens: 3000,
        };
        let wire = normalize_thinking_for_model("claude-sonnet-4-6", &enabled).unwrap();
        assert_eq!(wire["type"], "enabled");
        assert_eq!(wire["budget_tokens"], 3000);
    }

    #[test]
    fn thinking_sonnet_4_6_adaptive_preserved_with_explicit_display() {
        let adaptive = ThinkingConfig::Adaptive {
            display: Some(ThinkingDisplay::Omitted),
        };
        let wire = normalize_thinking_for_model("claude-sonnet-4-6", &adaptive).unwrap();
        assert_eq!(wire["type"], "adaptive");
        assert_eq!(wire["display"], "omitted");
    }

    #[test]
    fn build_request_body_emits_effort_in_output_config() {
        let provider =
            AnthropicProvider::new(AnthropicConfig::new("https://api.example.com", "cr_test"))
                .unwrap();
        let mut req = dummy_request();
        req.options.thinking = None;
        req.options.effort = Some(crate::domain::agent::types::EffortLevel::XHigh);
        let body = provider.build_request_body(&req);
        assert_eq!(body["output_config"]["effort"], "xhigh");
    }

    #[test]
    fn build_request_body_omits_output_config_when_no_effort() {
        let provider =
            AnthropicProvider::new(AnthropicConfig::new("https://api.example.com", "cr_test"))
                .unwrap();
        let mut req = dummy_request();
        req.options.effort = None;
        let body = provider.build_request_body(&req);
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn web_search_version_dispatched_by_model() {
        // 4.6+ → 新版 20260209
        assert_eq!(
            web_search_tool_version_for_model("claude-opus-4-7"),
            "web_search_20260209"
        );
        assert_eq!(
            web_search_tool_version_for_model("claude-opus-4-6"),
            "web_search_20260209"
        );
        assert_eq!(
            web_search_tool_version_for_model("claude-sonnet-4-6"),
            "web_search_20260209"
        );
        assert_eq!(
            web_search_tool_version_for_model("claude-mythos-preview"),
            "web_search_20260209"
        );
        // 老 / 不在新版支持矩阵的模型 → 维持 20250305
        assert_eq!(
            web_search_tool_version_for_model("claude-sonnet-4-5-20250929"),
            "web_search_20250305"
        );
        assert_eq!(
            web_search_tool_version_for_model("claude-opus-4-5-20251101"),
            "web_search_20250305"
        );
        assert_eq!(
            web_search_tool_version_for_model("claude-haiku-4-5-20251001"),
            "web_search_20250305"
        );
        assert_eq!(
            web_search_tool_version_for_model("claude-opus-4-1-20250805"),
            "web_search_20250305"
        );
    }

    #[test]
    fn build_request_body_uses_legacy_web_search_for_old_model() {
        let provider =
            AnthropicProvider::new(AnthropicConfig::new("https://api.example.com", "cr_test"))
                .unwrap();
        let mut req = dummy_request();
        req.options.model = "claude-sonnet-4-5-20250929".into();
        let body = provider.build_request_body(&req);
        let tools = body["tools"].as_array().unwrap();
        let ws = tools.iter().find(|t| t["name"] == "web_search").unwrap();
        assert_eq!(ws["type"], "web_search_20250305");
    }

    #[test]
    fn build_request_body_drops_thinking_for_haiku_compact() {
        // 摘要场景：compact 渠道用 haiku + 误传了 thinking——wire format 层兜底
        let provider =
            AnthropicProvider::new(AnthropicConfig::new("https://api.example.com", "cr_test"))
                .unwrap();
        let mut req = dummy_request();
        req.options.model = "claude-haiku-4-5-20251001".into();
        req.options.thinking = Some(ThinkingConfig::Adaptive { display: None });
        let body = provider.build_request_body(&req);
        assert!(
            body.get("thinking").is_none(),
            "haiku 上 thinking 必须被 drop"
        );
    }

    #[test]
    fn config_rejects_empty_token() {
        let err = AnthropicProvider::new(AnthropicConfig::new("https://x", "")).unwrap_err();
        assert!(matches!(err, ProviderError::Config(_)));
    }

    #[test]
    fn config_rejects_bad_url() {
        let err = AnthropicProvider::new(AnthropicConfig::new("ftp://x", "tok")).unwrap_err();
        assert!(matches!(err, ProviderError::Config(_)));
    }

    #[test]
    fn block_to_wire_image_uses_base64_source() {
        let block = Block::Image {
            mime: "image/png".into(),
            data: "iVBORw0".into(),
        };
        let v = block_to_wire(&block);
        assert_eq!(v["type"], "image");
        assert_eq!(v["source"]["type"], "base64");
        assert_eq!(v["source"]["media_type"], "image/png");
        assert_eq!(v["source"]["data"], "iVBORw0");
    }

    #[test]
    fn block_to_wire_server_tool_use_round_trips() {
        let block = Block::ToolUse {
            id: "srvtoolu_1".into(),
            name: "web_search".into(),
            input: json!({"query": "茅台"}),
            server_side: true,
        };
        let v = block_to_wire(&block);
        assert_eq!(v["type"], "server_tool_use");
        assert_eq!(v["name"], "web_search");
    }

    #[test]
    fn sse_decoder_assembles_simple_text_message() {
        let mut dec = SseDecoder::new();
        // message_start
        let _ = dec
            .consume(
                "message_start",
                r#"{"type":"message_start","message":{"id":"m1","model":"claude","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#,
            )
            .unwrap();
        // content_block_start (text)
        let _ = dec
            .consume(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            )
            .unwrap();
        // delta
        let ev = dec
            .consume(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
            )
            .unwrap();
        match ev {
            Some(ProviderEvent::TextDelta(s)) => assert_eq!(s, "hello"),
            other => panic!("expected TextDelta, got {other:?}"),
        }
        // stop
        let _ = dec
            .consume(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            )
            .unwrap();
        // message_delta with stop_reason
        let _ = dec
            .consume(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
            )
            .unwrap();
        // message_stop → MessageComplete
        let final_ev = dec
            .consume("message_stop", r#"{"type":"message_stop"}"#)
            .unwrap();
        match final_ev {
            Some(ProviderEvent::MessageComplete {
                message,
                stop_reason,
            }) => {
                assert_eq!(stop_reason, StopReason::EndTurn);
                assert_eq!(message.content.len(), 1);
                match &message.content[0] {
                    Block::Text { text, .. } => assert_eq!(text, "hello"),
                    _ => panic!("expected text block"),
                }
            }
            other => panic!("expected MessageComplete, got {other:?}"),
        }
    }

    #[test]
    fn sse_decoder_returns_protocol_error_on_index_skip() {
        // 协议保证 content_block_start 的 index 是连续递增的；跳号视作流损坏，
        // 必须返回 Protocol 错误而不是 panic（修复前 ensure_capacity 直接 panic）
        let mut dec = SseDecoder::new();
        let _ = dec
            .consume("message_start", r#"{"message":{"usage":{}}}"#)
            .unwrap();
        // 直接给 index=2，跳过 0、1
        let result = dec.consume(
            "content_block_start",
            r#"{"index":2,"content_block":{"type":"text","text":""}}"#,
        );
        match result {
            Err(ProviderError::Protocol(msg)) => assert!(msg.contains("跳号")),
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[test]
    fn sse_decoder_maps_overloaded_to_rate_limited() {
        // overloaded_error → RateLimited 让 RetryingProvider 退避重试
        let mut dec = SseDecoder::new();
        let result = dec.consume(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"too many requests"}}"#,
        );
        match result {
            Err(ProviderError::RateLimited(body)) => {
                assert!(body.contains("overloaded_error"));
                assert!(body.contains("too many requests"));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn sse_decoder_maps_rate_limit_error_to_rate_limited() {
        let mut dec = SseDecoder::new();
        let result = dec.consume(
            "error",
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
        );
        assert!(matches!(result, Err(ProviderError::RateLimited(_))));
    }

    #[test]
    fn sse_decoder_maps_api_error_to_transient() {
        // api_error 是 5xx 类——交给 retry 兜
        let mut dec = SseDecoder::new();
        let result = dec.consume(
            "error",
            r#"{"type":"error","error":{"type":"api_error","message":"upstream blew up"}}"#,
        );
        assert!(matches!(result, Err(ProviderError::Transient(_))));
    }

    #[test]
    fn sse_decoder_maps_invalid_request_to_request_400() {
        let mut dec = SseDecoder::new();
        let result = dec.consume(
            "error",
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad model"}}"#,
        );
        match result {
            Err(ProviderError::Request { status, body }) => {
                assert_eq!(status, 400);
                assert!(body.contains("invalid_request_error"));
            }
            other => panic!("expected Request{{400}}, got {other:?}"),
        }
    }

    #[test]
    fn sse_decoder_maps_unknown_error_type_to_transient() {
        // 未知 type 走 Transient——给 retry 一次机会，比盲目 4xx 拒绝好
        let mut dec = SseDecoder::new();
        let result = dec.consume(
            "error",
            r#"{"type":"error","error":{"type":"future_unknown_error","message":"???"}}"#,
        );
        assert!(matches!(result, Err(ProviderError::Transient(_))));
    }

    #[test]
    fn sse_decoder_unwraps_relay_404_with_inner_not_found() {
        // claude-relay 形态 C：上游 404 被 relay 包装。我们要从 details 里挖出
        // 真实模型名供用户定位。
        let mut dec = SseDecoder::new();
        let payload = r#"{"error":"Claude API error","status":404,"details":"{\"type\":\"error\",\"error\":{\"type\":\"not_found_error\",\"message\":\"model: claude-sonnet-4-7\"}}","timestamp":"2026-05-09T16:27:03.489Z"}"#;
        let result = dec.consume("error", payload);
        match result {
            Err(ProviderError::Request { status, body }) => {
                assert_eq!(status, 404);
                assert!(
                    body.contains("claude-sonnet-4-7"),
                    "body 必须含真实模型名: {body}"
                );
                assert!(body.contains("not_found_error"));
            }
            other => panic!("expected Request{{404}}, got {other:?}"),
        }
    }

    #[test]
    fn sse_decoder_relay_wrapped_429_maps_to_rate_limited() {
        let mut dec = SseDecoder::new();
        let payload = r#"{"error":"upstream","status":429,"details":"slow down"}"#;
        let result = dec.consume("error", payload);
        assert!(matches!(result, Err(ProviderError::RateLimited(_))));
    }

    #[test]
    fn sse_decoder_relay_wrapped_5xx_maps_to_transient() {
        let mut dec = SseDecoder::new();
        let payload = r#"{"error":"upstream","status":502,"details":"bad gateway"}"#;
        let result = dec.consume("error", payload);
        assert!(matches!(result, Err(ProviderError::Transient(_))));
    }

    #[test]
    fn sse_decoder_emits_usage_event_from_message_delta() {
        let mut dec = SseDecoder::new();
        let _ = dec
            .consume(
                "message_start",
                r#"{"message":{"usage":{"input_tokens":100,"cache_read_input_tokens":50,"output_tokens":0}}}"#,
            )
            .unwrap();
        let _ = dec
            .consume(
                "content_block_start",
                r#"{"index":0,"content_block":{"type":"text","text":""}}"#,
            )
            .unwrap();
        // 普通 delta
        let _ = dec
            .consume(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            )
            .unwrap();
        let _ = dec.consume("content_block_stop", r#"{"index":0}"#).unwrap();
        // message_delta 必须 emit Usage（修复前 Usage 在 message_stop 才合并，被丢弃）
        let usage_ev = dec
            .consume(
                "message_delta",
                r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#,
            )
            .unwrap();
        match usage_ev {
            Some(ProviderEvent::Usage(u)) => {
                assert_eq!(u.input_tokens, 100);
                assert_eq!(u.output_tokens, 42);
                assert_eq!(u.cache_read_tokens, 50);
            }
            other => panic!("expected Usage event, got {other:?}"),
        }
    }

    #[test]
    fn sse_decoder_assembles_tool_use_with_streamed_input() {
        let mut dec = SseDecoder::new();
        let _ = dec.consume(
            "message_start",
            r#"{"message":{"usage":{"input_tokens":10,"output_tokens":0}}}"#,
        );
        // tool_use 块开启
        let _ = dec.consume(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_quote","input":{}}}"#,
        );
        // input 流式分片
        let _ = dec.consume(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"co"}}"#,
        );
        let _ = dec.consume(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"de\":\"600519\"}"}}"#,
        );
        let _ = dec.consume("content_block_stop", r#"{"index":0}"#);
        let _ = dec.consume(
            "message_delta",
            r#"{"delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}"#,
        );
        let final_ev = dec.consume("message_stop", r#"{}"#).unwrap();
        match final_ev {
            Some(ProviderEvent::MessageComplete { message, .. }) => {
                let block = &message.content[0];
                match block {
                    Block::ToolUse {
                        name,
                        input,
                        server_side,
                        ..
                    } => {
                        assert_eq!(name, "get_quote");
                        assert_eq!(input["code"], "600519");
                        assert!(!server_side);
                    }
                    _ => panic!("expected tool_use"),
                }
            }
            other => panic!("expected MessageComplete, got {other:?}"),
        }
    }
}
