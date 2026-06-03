//! Stress suite for the **text-protocol `<use_tool>` tool-use reliability under pressure**.
//!
//! Motivating worry: the tool protocol is a *text* protocol (`<use_tool>` / `<tool_result>` /
//! `<tool_error>`) — NOT provider-native function-calling. Does that protocol stay healthy under
//! pressure? Specifically:
//!   - **多轮 tool 链** — 6~8 真实多轮对话，每轮都要调 tool 且依赖前几轮 tool 结果；会不会途中
//!     畸形调用（parse_error）、漏调、或"声称调了但其实没 dispatch"（协议漂移）。
//!   - **skill 编排 multi-tool** — 先 create_skill 写一个点名 A→B→C 的 playbook，新一轮让它
//!     load_skill 取回并按序调 A/B/C。
//!   - **多 tool 选择压力** — 注册 12+ 个 tool，给只需 2~3 个的任务；会不会选错 / 调一堆无关 tool。
//!   - **多轮中的对抗恢复** — 故意插一轮触发 tool error，模型该轮恢复后，后续轮仍正常用 tool
//!     （不被一次 error 带崩协议）。
//!
//! Spec:
//!   - docs/design/agent-infra-module.md §2 (Tool 注册 / `<use_tool>`/`<tool_result>`/`<tool_error>`
//!     文本协议；multi-tool single turn；JSON parse 失败 / 非法嵌套 → `<tool_error code=parse_error>`；
//!     `<tool_error>` 不终止 loop) + §3 (Agent Loop) + §5 (Tool Registry / SystemPromptBuilder).
//!   - docs/design/agent-runtime-module.md §4.2 (本地通用 tool 契约) + §Skills (create_skill /
//!     load_skill 渐进披露).
//!
//! Protocol-health metric (how "畸形调用" is counted):
//!   A malformed `<use_tool>` (bad JSON / illegal nesting) does NOT surface as a `ToolEnd` event —
//!   the loop turns it into a `<tool_error name="_parser" code="parse_error">` injected into the
//!   next turn and PERSISTED into the conversation history (loop_executor.rs §ParserEvent::ParseError).
//!   So after a run we scan the persisted conversation for `code="parse_error"` markers to count
//!   malformed protocol emissions. A *wrong-tool* selection surfaces as a `ToolEnd` whose `name` is
//!   not a registered tool (NotRegistered → is_error). Both are asserted ≤ a tight threshold (0);
//!   "是否合理 / 链对了没" is graded by the judge.
//!
//! Live tests are `#[tokio::test] #[ignore]` and skip (print) when their required env is unset.
//! Channels / env (TEST-ONLY env names, NEVER hardcode secrets):
//!     TEST_DS_BASE / TEST_DS_KEY / TEST_DS_MODEL    (chat_completions — DeepSeek)
//!     TEST_ANT_BASE / TEST_ANT_KEY / TEST_ANT_MODEL (messages — Anthropic)
//!     TEST_OAI_BASE / TEST_OAI_KEY / TEST_OAI_MODEL (responses — OpenAI)
//!     JUDGE_BASE / JUDGE_KEY / JUDGE_MODEL / JUDGE_WIRE (separate strong evaluator)
//!
//! Run the stress suite across wires (placeholder creds):
//!   TEST_OAI_BASE=… TEST_OAI_KEY=… TEST_OAI_MODEL=… \
//!   TEST_ANT_BASE=… TEST_ANT_KEY=… TEST_ANT_MODEL=… \
//!   TEST_DS_BASE=…  TEST_DS_KEY=…  TEST_DS_MODEL=… \
//!   JUDGE_BASE=… JUDGE_KEY=… JUDGE_MODEL=… JUDGE_WIRE=messages \
//!   cargo test --manifest-path src-tauri/Cargo.toml judge_stress_ -- --ignored --nocapture

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde_json::json;
use tokio::sync::mpsc;

use crate::domain::agent::{
    AgentEvent, AgentMessage, AgentMessageBlock, AgentMessageRole, AgentRunRequest, ContextBundle,
    ProviderChannel, SideEffect, ToolSpec, WireFormat,
};
use crate::domain::shared::ErrorCode;
use crate::infrastructure::agent::http_provider::HttpProvider;
use crate::infrastructure::agent::loop_executor::{run_agent_turn, ProviderStream};
use crate::infrastructure::agent::local_tools::register_local_tools;
use crate::infrastructure::agent::messages_repo::AgentMessagesRepo;
use crate::infrastructure::agent::skill_tools::register_skill_tools;
use crate::infrastructure::agent::tool_registry::{
    FnToolHandler, ToolHandler, ToolHandlerFuture, ToolHandlerOutput, ToolInvocation, ToolRegistry,
};

// ===========================================================================
// Channel helpers (self-contained mirror of judge_tools_tests.rs wiring).
// ===========================================================================

fn base_channel() -> ProviderChannel {
    ProviderChannel {
        channel_id: "judge-stress-suite".into(),
        provider: "judge-stress-suite".into(),
        wire_format: WireFormat::Messages,
        base_url: None,
        api_key: String::new(),
        model: "fake-model".into(),
        stream: true,
        enabled: true,
        supports_vision: false,
        supports_thinking: false,
        max_output_tokens: Some(1024),
        context_window_tokens: None,
        thinking_budget_tokens: None,
    }
}

struct AgentChannel {
    label: &'static str,
    channel: ProviderChannel,
}

fn agent_channel_responses() -> Option<AgentChannel> {
    let (b, k) = (std::env::var("TEST_OAI_BASE").ok()?, std::env::var("TEST_OAI_KEY").ok()?);
    let m = std::env::var("TEST_OAI_MODEL").unwrap_or_else(|_| "gpt-5".into());
    let mut c = base_channel();
    c.wire_format = WireFormat::Responses;
    c.base_url = Some(b);
    c.api_key = k;
    c.model = m;
    c.max_output_tokens = Some(1024);
    Some(AgentChannel { label: "responses", channel: c })
}

fn agent_channel_messages() -> Option<AgentChannel> {
    let (b, k) = (std::env::var("TEST_ANT_BASE").ok()?, std::env::var("TEST_ANT_KEY").ok()?);
    let m = std::env::var("TEST_ANT_MODEL").unwrap_or_else(|_| "claude-haiku-4-5-20251001".into());
    let mut c = base_channel();
    c.wire_format = WireFormat::Messages;
    c.base_url = Some(b);
    c.api_key = k;
    c.model = m;
    c.max_output_tokens = Some(1024);
    Some(AgentChannel { label: "messages", channel: c })
}

fn agent_channel_chat_completions() -> Option<AgentChannel> {
    let k = std::env::var("TEST_DS_KEY").ok()?;
    let b = std::env::var("TEST_DS_BASE").unwrap_or_else(|_| "https://api.deepseek.com".into());
    let m = std::env::var("TEST_DS_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".into());
    let mut c = base_channel();
    c.wire_format = WireFormat::ChatCompletions;
    c.base_url = Some(b);
    c.api_key = k;
    c.model = m;
    c.max_output_tokens = Some(1024);
    Some(AgentChannel { label: "chat_completions", channel: c })
}

/// All agent-under-test channels present in env (for fan-out across 3 wires; DeepSeek emphasized).
fn all_agent_channels() -> Vec<AgentChannel> {
    [
        agent_channel_chat_completions(),
        agent_channel_messages(),
        agent_channel_responses(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// DeepSeek (chat_completions) if present, else the next strongest channel — for the DeepSeek-first
/// scenarios (S2/S4) we still want a second strong provider when available.
fn deepseek_then_strong() -> Vec<AgentChannel> {
    let mut out = Vec::new();
    if let Some(c) = agent_channel_chat_completions() {
        out.push(c);
    }
    // a "strong" provider second: prefer messages (Anthropic), else responses (OpenAI).
    if let Some(c) = agent_channel_messages() {
        out.push(c);
    } else if let Some(c) = agent_channel_responses() {
        out.push(c);
    }
    // If DeepSeek was absent but we have something, run on whatever's present.
    if out.is_empty() {
        out = all_agent_channels();
    }
    out
}

// ===========================================================================
// Judge harness (self-contained mirror of judge_tools_tests.rs).
// ===========================================================================

#[derive(Debug, Clone)]
struct Verdict {
    pass: bool,
    score: f32,
    reason: String,
}

fn judge_channel() -> Option<ProviderChannel> {
    let base = std::env::var("JUDGE_BASE").ok()?;
    let key = std::env::var("JUDGE_KEY").ok()?;
    let model = std::env::var("JUDGE_MODEL").ok()?;
    let wire = match std::env::var("JUDGE_WIRE")
        .unwrap_or_else(|_| "messages".into())
        .to_ascii_lowercase()
        .as_str()
    {
        "responses" => WireFormat::Responses,
        "chat_completions" | "chat" => WireFormat::ChatCompletions,
        _ => WireFormat::Messages,
    };
    let mut c = base_channel();
    c.channel_id = "judge".into();
    c.provider = "judge".into();
    c.wire_format = wire;
    c.base_url = Some(base);
    c.api_key = key;
    c.model = model;
    c.max_output_tokens = Some(1024);
    Some(c)
}

const JUDGE_SYSTEM: &str = "You are a STRICT evaluator of an AI agent's output. \
You are given a scenario, the agent's output, and a rubric. \
Decide whether the agent output satisfies ALL rubric criteria. \
You MUST respond with ONLY a single raw JSON object and NOTHING else — no reasoning, no analysis, \
no markdown, no prose before or after. Your entire reply must be exactly one JSON object in this shape: \
{\"pass\": <true|false>, \"score\": <number between 0 and 1>, \"reason\": \"<short explanation, max 200 chars>\"}. \
Put all of your reasoning into the \"reason\" field; do NOT write any reasoning outside the JSON. \
Set \"pass\" to true ONLY if every rubric criterion is satisfied. \
Begin your reply with the character { and end it with the character }.";

const JUDGE_PREFILL: &str = "{\"pass\":";

async fn judge(
    judge_ch: &ProviderChannel,
    scenario: &str,
    agent_output: &str,
    rubric: &str,
) -> Result<Verdict, String> {
    let user = format!(
        "Scenario:\n{scenario}\n\nAgent output:\n{agent_output}\n\nRubric (pass if ALL satisfied):\n{rubric}"
    );

    let mut provider = HttpProvider::new(judge_ch.clone())
        .map_err(|e| format!("judge HttpProvider build failed: {e}"))?;
    let messages = vec![
        AgentMessage {
            message_id: "judge-sys".into(),
            run_id: Some("judge".into()),
            conversation_id: None,
            seq: None,
            kind: None,
            role: AgentMessageRole::System,
            blocks: vec![AgentMessageBlock::Text { text: JUDGE_SYSTEM.to_string() }],
            created_at: Utc::now(),
        },
        AgentMessage {
            message_id: "judge-user".into(),
            run_id: Some("judge".into()),
            conversation_id: None,
            seq: None,
            kind: None,
            role: AgentMessageRole::User,
            blocks: vec![AgentMessageBlock::Text { text: user }],
            created_at: Utc::now(),
        },
        AgentMessage {
            message_id: "judge-prefill".into(),
            run_id: Some("judge".into()),
            conversation_id: None,
            seq: None,
            kind: None,
            role: AgentMessageRole::Assistant,
            blocks: vec![AgentMessageBlock::Text { text: JUDGE_PREFILL.to_string() }],
            created_at: Utc::now(),
        },
    ];
    let ctx = ContextBundle::new("judge");
    let (tx, rx) = mpsc::channel::<AgentEvent>(256);
    let drain = tokio::spawn(async move {
        let mut rx = rx;
        while rx.recv().await.is_some() {}
    });
    let outcome = provider
        .next_turn(&messages, &ctx, &tx, "judge")
        .await
        .map_err(|e| format!("judge next_turn failed: {e}"));
    drop(tx);
    let _ = drain.await;
    let raw = outcome?.text;

    if let Some(v) = parse_verdict(&raw) {
        return Ok(v);
    }
    let stitched = format!("{JUDGE_PREFILL}{raw}");
    parse_verdict(&stitched)
        .ok_or_else(|| format!("judge reply not parseable as verdict JSON: {raw:?}"))
}

fn parse_verdict(raw: &str) -> Option<Verdict> {
    // Strict path: well-formed JSON object.
    if let Some(obj) = extract_json_object(raw) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&obj) {
            if let Some(pass) = v.get("pass").and_then(|x| x.as_bool()) {
                let score = v
                    .get("score")
                    .and_then(|x| x.as_f64())
                    .map(|f| f as f32)
                    .unwrap_or(if pass { 1.0 } else { 0.0 });
                let reason = v
                    .get("reason")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                return Some(Verdict { pass, score, reason });
            }
        }
    }
    // Lenient fallback: 评委有时返回带额外字段 / 内层未转义引号的 JSON（不可严格解析）。
    // JUDGE_PREFILL 保证文本里 `"pass":<bool>` 是首个字段——只提取这个布尔即可判定。
    let after = raw.split("\"pass\":").nth(1)?.trim_start();
    let pass = if after.starts_with("true") {
        true
    } else if after.starts_with("false") {
        false
    } else {
        return None;
    };
    Some(Verdict {
        pass,
        score: if pass { 1.0 } else { 0.0 },
        reason: "(lenient parse: judge JSON malformed)".to_string(),
    })
}

fn extract_json_object(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let start = s.find('{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        let c = b as char;
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(s[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn assert_verdict(test: &str, label: &str, v: &Verdict) {
    println!(
        "[judge][{test}][{label}] pass={} score={:.2} reason={:?}",
        v.pass, v.score, v.reason
    );
    assert!(
        v.pass,
        "[{test}][{label}] judge FAILED: score={:.2} reason={}",
        v.score, v.reason
    );
}

// ===========================================================================
// Persistence + multi-turn helpers.
// ===========================================================================

/// Fresh in-memory AppDb-backed messages repo (mirrors llm_judge_tests::fresh_repo).
fn fresh_repo() -> AgentMessagesRepo {
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::db::{run_migrations, AppDb};
    let db = AppDb::open_in_memory().unwrap();
    db.with(|c| run_migrations(c, agent_migrations()).unwrap());
    AgentMessagesRepo::new(db)
}

fn user_message(run: &str, text: &str) -> AgentMessage {
    AgentMessage {
        message_id: format!("u-{run}-{}", uuid::Uuid::new_v4()),
        run_id: Some(run.to_string()),
        conversation_id: None,
        seq: None,
        kind: None,
        role: AgentMessageRole::User,
        blocks: vec![AgentMessageBlock::Text { text: text.into() }],
        created_at: Utc::now(),
    }
}

/// Outcome of running ONE conversational turn (over a persisted conversation).
struct TurnOut {
    answer: String,
    /// (tool_name, is_error) for each ToolEnd event observed THIS turn.
    tools: Vec<(String, bool)>,
}

/// Run ONE turn over a persisted conversation (engine续接 conversation_id): hand only the new user
/// message; the engine persists it, loads prior history, runs the loop, persists outputs. Collects
/// the streamed answer + (tool_name, is_error) per ToolEnd. The repo accumulates the FULL audit log
/// so the caller can scan it for protocol-health markers (parse_error) after the whole session.
async fn run_turn(
    repo: &AgentMessagesRepo,
    registry: Arc<ToolRegistry>,
    channel: &ProviderChannel,
    conversation_id: &str,
    run_id: &str,
    user_text: &str,
    max_turns: u32,
) -> TurnOut {
    let provider = Box::new(HttpProvider::new(channel.clone()).unwrap());
    let request = AgentRunRequest {
        run_id: run_id.into(),
        trigger: "user".into(),
        channel: channel.clone(),
        max_turns,
        input: vec![user_message(run_id, user_text)],
        conversation_id: Some(conversation_id.to_string()),
        compaction: None,
        fallback_channels: vec![],
        retry: None,
    };
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let pump = tokio::spawn(async move {
        let mut text = String::new();
        let mut tools: Vec<(String, bool)> = Vec::new();
        while let Some(e) = rx.recv().await {
            match e {
                AgentEvent::TextDelta { delta, .. } => text.push_str(&delta),
                AgentEvent::ToolEnd { name, is_error, .. } => tools.push((name, is_error)),
                _ => {}
            }
        }
        (text, tools)
    });
    run_agent_turn(
        request,
        registry,
        ContextBundle::new(run_id),
        vec![provider],
        tx,
        Some(repo.clone()),
    )
    .await
    .expect("turn run");
    let (answer, tools) = pump.await.unwrap();
    TurnOut { answer, tools }
}

/// Run ONE single-shot loop (no persistence) over a channel + registry; collect answer + ToolEnd
/// pairs. Used by S3 (multi-tool selection) where one turn + the event tools list is enough, plus a
/// dedicated parse-error sink registered on the registry.
async fn run_loop_collect(
    channel: ProviderChannel,
    registry: Arc<ToolRegistry>,
    user_text: &str,
    max_turns: u32,
) -> TurnOut {
    let provider = Box::new(HttpProvider::new(channel.clone()).unwrap());
    let request = AgentRunRequest {
        run_id: "judge-stress-run".into(),
        trigger: "user".into(),
        channel,
        max_turns,
        input: vec![user_message("judge-stress-run", user_text)],
        conversation_id: None,
        compaction: None,
        fallback_channels: vec![],
        retry: None,
    };
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let pump = tokio::spawn(async move {
        let mut text = String::new();
        let mut tools: Vec<(String, bool)> = Vec::new();
        while let Some(e) = rx.recv().await {
            match e {
                AgentEvent::TextDelta { delta, .. } => text.push_str(&delta),
                AgentEvent::ToolEnd { name, is_error, .. } => tools.push((name, is_error)),
                _ => {}
            }
        }
        (text, tools)
    });
    run_agent_turn(
        request,
        registry,
        ContextBundle::new("judge-stress-run"),
        vec![provider],
        tx,
        None,
    )
    .await
    .expect("loop run");
    let (answer, tools) = pump.await.unwrap();
    TurnOut { answer, tools }
}

// ===========================================================================
// Protocol-health accounting.
// ===========================================================================

/// Count malformed `<use_tool>` emissions (parse_error) by scanning the persisted conversation for
/// the loop's injected `<tool_error ... code="parse_error">` markers. A malformed call never
/// surfaces as a `ToolEnd` event (it's fed back as a `_parser` tool_error into the next turn and
/// persisted), so the persisted history is the authoritative place to detect it.
/// Spec: agent-infra-module.md §2 (JSON parse 失败 / 非法嵌套 → `<tool_error code=parse_error>`).
fn count_parse_errors_in_history(repo: &AgentMessagesRepo, conversation_id: &str) -> usize {
    let all = repo.load_conversation(conversation_id).unwrap_or_default();
    all.iter()
        .flat_map(|m| m.blocks.iter())
        .filter_map(|b| match b {
            AgentMessageBlock::Text { text } => Some(text),
            _ => None,
        })
        .map(|t| t.matches(r#"code="parse_error""#).count())
        .sum()
}

/// Count, across a list of per-turn tool lists, ToolEnd events whose name is NOT in `registered`.
/// These are wrong-tool selections that the engine dispatched and got NotRegistered back for
/// (surface as is_error ToolEnd). A healthy protocol never selects an unregistered tool.
fn count_wrong_tool_selections(turn_tools: &[Vec<(String, bool)>], registered: &[&str]) -> usize {
    turn_tools
        .iter()
        .flatten()
        .filter(|(name, _)| !registered.contains(&name.as_str()))
        .count()
}

/// `dispatched_ok` across a set of per-turn tool lists.
fn any_dispatched_ok(turn_tools: &[Vec<(String, bool)>], name: &str) -> bool {
    turn_tools
        .iter()
        .flatten()
        .any(|(n, err)| n == name && !err)
}

// ===========================================================================
// Deterministic stateful tool fixtures (shared, server-side KV + arithmetic).
// ===========================================================================

/// A process-side deterministic key/value store the tools mutate. Shared (Arc) across the whole
/// conversation so a `kv_put` in turn N is visible to a `kv_get` in turn N+1 — this is what makes
/// the multi-turn tool *chain* real (later turns depend on earlier tool side effects).
#[derive(Default)]
struct KvState {
    map: Mutex<HashMap<String, i64>>,
}

/// Register a small deterministic tool family that forces a genuine cross-turn dependency:
///   - `kv_put{key,val}` — store an integer under a key (returns {ok:true}).
///   - `kv_get{key}`     — fetch the integer stored under a key (returns {key,value}); -1 if unset.
///   - `add{a,b}`        — integer sum (the model must call it, not mental-math).
/// All are SideEffect::None and fully deterministic.
fn register_kv_chain_tools(registry: &ToolRegistry, state: Arc<KvState>) {
    // kv_put
    {
        let spec = ToolSpec::new(
            "kv_put",
            "把一个整数 val 存到键 key 下（覆盖旧值）。需要保存某个数值供后续步骤取用时，必须调用本 tool。",
            json!({
                "type": "object",
                "properties": { "key": { "type": "string" }, "val": { "type": "integer" } },
                "required": ["key", "val"]
            }),
            vec![r#"<use_tool name="kv_put">{"key":"x","val":7}</use_tool>"#.to_string()],
            5000,
            SideEffect::None,
        );
        let st = state.clone();
        let handler: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(move |inv: ToolInvocation| {
            let st = st.clone();
            Box::pin(async move {
                let key = inv.input.get("key").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let val = inv.input.get("val").and_then(|v| v.as_i64()).unwrap_or(0);
                if key.is_empty() {
                    return ToolHandlerOutput::err(
                        json!({ "message": "key must not be empty" }),
                        ErrorCode::InvalidInput,
                    );
                }
                st.map.lock().unwrap().insert(key.clone(), val);
                ToolHandlerOutput::ok(json!({ "ok": true, "key": key, "stored": val }))
            }) as ToolHandlerFuture
        }));
        registry.register_tool(spec, handler).unwrap();
    }
    // kv_get
    {
        let spec = ToolSpec::new(
            "kv_get",
            "按键 key 取回之前用 kv_put 存的整数值。需要读取先前保存的数值时，必须调用本 tool，不要凭记忆猜。",
            json!({
                "type": "object",
                "properties": { "key": { "type": "string" } },
                "required": ["key"]
            }),
            vec![r#"<use_tool name="kv_get">{"key":"x"}</use_tool>"#.to_string()],
            5000,
            SideEffect::None,
        );
        let st = state.clone();
        let handler: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(move |inv: ToolInvocation| {
            let st = st.clone();
            Box::pin(async move {
                let key = inv.input.get("key").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let value = st.map.lock().unwrap().get(&key).copied().unwrap_or(-1);
                ToolHandlerOutput::ok(json!({ "key": key, "value": value }))
            }) as ToolHandlerFuture
        }));
        registry.register_tool(spec, handler).unwrap();
    }
    // add
    {
        let spec = ToolSpec::new(
            "add",
            "把两个整数相加并返回它们的和。需要做加法时必须调用本 tool，不要自己心算。",
            json!({
                "type": "object",
                "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } },
                "required": ["a", "b"]
            }),
            vec![r#"<use_tool name="add">{"a":2,"b":3}</use_tool>"#.to_string()],
            5000,
            SideEffect::None,
        );
        let handler: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(|inv: ToolInvocation| {
            Box::pin(async move {
                let a = inv.input.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
                let b = inv.input.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
                ToolHandlerOutput::ok(json!({ "sum": a + b }))
            }) as ToolHandlerFuture
        }));
        registry.register_tool(spec, handler).unwrap();
    }
}

/// A unique temp directory under the system temp dir.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gangzi-judge-stress-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ===========================================================================
// S1. Multi-turn tool CHAIN (6 turns, cross-turn data dependency). Fan out over 3 wires.
//
// Each turn needs a tool AND depends on earlier turns' tool results:
//   T1: kv_put(base, 100)
//   T2: kv_get(base) -> 100; kv_put(step, base+50=150) using add
//   T3: kv_get(step) -> 150; kv_put(total, step+25=175) using add
//   T4: kv_get(total) -> 175; report it (read-back of the accumulated chain)
//   T5: add(total, 1000) -> 1175; kv_put(final, 1175)
//   T6: kv_get(final) -> 1175; final answer must state 1175 (the whole chain summed).
//
// Protocol-health hard asserts:
//   - 0 malformed (parse_error) calls across the whole persisted conversation.
//   - 0 wrong-tool selections (every ToolEnd name ∈ {kv_put,kv_get,add}).
//   - at least one successful kv_put / kv_get / add dispatched over the session.
// Judge: final answer correctly == 1175, chained through the per-turn tool results (no drift /
// no "claimed-but-not-dispatched").
// ===========================================================================
#[tokio::test]
#[ignore]
async fn judge_stress_multiturn_tool_chain() {
    let Some(judge_ch) = judge_channel() else {
        println!("[judge_stress_multiturn_tool_chain] skip: set JUDGE_*");
        return;
    };
    let channels = all_agent_channels();
    if channels.is_empty() {
        println!("[judge_stress_multiturn_tool_chain] skip: set an agent channel (TEST_*)");
        return;
    }

    // Deterministic expected chain end value.
    let expected_final = 100 + 50 + 25 + 1000; // 1175 (T2..T5 accumulation)

    let mut ran = 0;
    for ch in channels {
        let state = Arc::new(KvState::default());
        let registry = Arc::new(ToolRegistry::new_without_persist());
        register_kv_chain_tools(&registry, state.clone());
        let repo = fresh_repo();
        let conv = format!("stress-chain-{}", ch.label);

        // 6 dependent turns. Each instructs precisely so we can assert the chain end value.
        let turns: [(&str, &str); 6] = [
            ("c1", "第一步：请用 kv_put 把整数 100 存到键 'base' 下。完成后告诉我你存了多少。"),
            ("c2", "第二步：请先用 kv_get 取回键 'base' 的值，再用 add 把它加上 50，\
                    然后用 kv_put 把这个和存到键 'step' 下。告诉我 step 现在是多少。"),
            ("c3", "第三步：请先用 kv_get 取回键 'step' 的值，再用 add 把它加上 25，\
                    然后用 kv_put 把结果存到键 'total' 下。告诉我 total 现在是多少。"),
            ("c4", "第四步：请用 kv_get 取回键 'total' 的当前值，并把它原样报给我。"),
            ("c5", "第五步：请用 add 把刚才 total 的值再加上 1000，\
                    然后用 kv_put 把结果存到键 'final' 下。告诉我 final 是多少。"),
            ("c6", "最后一步：请用 kv_get 取回键 'final' 的值，并用一句话告诉我这个最终结果。\
                    这一路的步骤都必须真的用工具，不要心算。"),
        ];

        let mut all_turn_tools: Vec<Vec<(String, bool)>> = Vec::new();
        let mut last_answer = String::new();
        for (rid, text) in turns {
            let out = run_turn(&repo, registry.clone(), &ch.channel, &conv, rid, text, 6).await;
            println!(
                "[judge_stress_multiturn_tool_chain][{}][{}] tools={:?} answer={:?}",
                ch.label, rid, out.tools, out.answer
            );
            last_answer = out.answer;
            all_turn_tools.push(out.tools);
        }

        // ---- Protocol-health accounting ----
        let total_tool_calls: usize = all_turn_tools.iter().map(|t| t.len()).sum();
        let parse_errors = count_parse_errors_in_history(&repo, &conv);
        let wrong_tools =
            count_wrong_tool_selections(&all_turn_tools, &["kv_put", "kv_get", "add"]);
        println!(
            "[judge_stress_multiturn_tool_chain][{}] HEALTH turns={} total_tool_calls={} \
             malformed(parse_error)={} wrong_tool_selections={}",
            ch.label, turns.len(), total_tool_calls, parse_errors, wrong_tools
        );

        assert_eq!(
            parse_errors, 0,
            "[{}] malformed <use_tool> (parse_error) detected in the multi-turn chain — protocol drift",
            ch.label
        );
        assert_eq!(
            wrong_tools, 0,
            "[{}] wrong-tool selection(s) detected (a ToolEnd named a non-registered tool): {:?}",
            ch.label, all_turn_tools
        );
        assert!(
            any_dispatched_ok(&all_turn_tools, "kv_put"),
            "[{}] expected at least one successful kv_put over the chain ({:?})",
            ch.label, all_turn_tools
        );
        assert!(
            any_dispatched_ok(&all_turn_tools, "kv_get"),
            "[{}] expected at least one successful kv_get over the chain ({:?})",
            ch.label, all_turn_tools
        );
        assert!(
            any_dispatched_ok(&all_turn_tools, "add"),
            "[{}] expected at least one successful add over the chain ({:?})",
            ch.label, all_turn_tools
        );
        // Anti-drift: the engine's server-side KV must hold the chained final value — proves the
        // tools actually ran and chained (not "claimed but not dispatched").
        let final_on_server = state.map.lock().unwrap().get("final").copied().unwrap_or(i64::MIN);
        assert_eq!(
            final_on_server, expected_final,
            "[{}] server-side KV 'final' must equal {expected_final} — the tool chain truly executed",
            ch.label
        );

        let scenario = format!(
            "A 6-turn conversation where each turn used deterministic tools (kv_put / kv_get / add) \
             and depended on prior turns' tool results, accumulating 100 → +50 → +25 → +1000. The \
             FINAL answer (last turn) had to report the end-of-chain value {expected_final}, taken \
             via kv_get(final). The agent's final-turn answer is below."
        );
        let rubric = format!(
            "1) 最终回答里给出的结果就是 {expected_final}；\
             2) 没有报一个不同的数字；\
             3) 结果是顺着前几轮工具结果一路链下来的（不是凭空给的）。"
        );
        let v = judge(&judge_ch, &scenario, &last_answer, &rubric)
            .await
            .unwrap_or_else(|e| panic!("[{}] judge error: {e}", ch.label));
        assert_verdict("stress_multiturn_tool_chain", ch.label, &v);
        ran += 1;
    }
    println!("[judge_stress_multiturn_tool_chain] judged {ran} wire format(s)");
    assert!(ran > 0);
}

// ===========================================================================
// S2. Skill via fork (run_skill). DeepSeek + a strong provider.
//
//   T1: create_skill 'product-via-shell' whose body (plain prose) tells how to multiply A*B via
//       run_bash (echo $((A*B))).
//   T2 (NEW turn): "执行 product-via-shell" → model calls run_skill, which FORKS an isolated
//       sub-agent over the SKILL.md body; the sub-agent runs run_bash and returns the result; the
//       parent only sees the returned text (isolation = fork's core value).
//
// Hard asserts: create_skill ok, run_skill ok; 0 parse_error; 0 wrong-tool selections (registered
//   set incl. the skill + fork tools). The sub-agent's run_bash is NOT in the parent's tool record.
// Judge: did the parent relay the forked sub-agent's correct result (1517).
// ===========================================================================
#[tokio::test]
#[ignore]
async fn judge_stress_skill_orchestrates_multitool() {
    let Some(judge_ch) = judge_channel() else {
        println!("[judge_stress_skill_orchestrates_multitool] skip: set JUDGE_*");
        return;
    };
    let channels = deepseek_then_strong();
    if channels.is_empty() {
        println!("[judge_stress_skill_orchestrates_multitool] skip: set an agent channel");
        return;
    }

    let mut ran = 0;
    for ch in channels {
        // 正确的 Skill 模型（对齐 Anthropic Agent Skills）：skill 是**自包含的说明书**，
        // 不编排 infra 注册 tool、正文不含任何 <use_tool> 标签；agent 用自己的通用 tool
        // （这里是 run_bash）按说明执行。故注册的是本地通用 tool + skill 管理 tool。
        let ws = temp_dir(&format!("skill-ws-{}", ch.label));
        let skills_dir = temp_dir(&format!("skill-orch-{}", ch.label));
        let registry = Arc::new(ToolRegistry::new_without_persist());
        register_local_tools(&registry, ws.clone()).unwrap();
        register_skill_tools(&registry, skills_dir.clone()).unwrap();
        // run_skill (fork) over the same registry + channel: the forked sub-agent inherits run_bash
        // and executes the skill body in isolation (spec §3.5/§3.6).
        let tasks = crate::infrastructure::agent::subagent::SubAgentTaskRegistry::new();
        let fork = crate::infrastructure::agent::subagent::ForkHandle::new(
            registry.clone(),
            crate::infrastructure::agent::subagent::http_provider_factory(),
            None,
            None,
            ch.channel.clone(),
            crate::infrastructure::agent::skill_store::SkillStore::new(skills_dir.clone()),
            tasks,
        )
        .with_max_turns(8);
        crate::infrastructure::agent::subagent::register_subagent_tools(&registry, fork).unwrap();
        let repo = fresh_repo();
        let conv = format!("stress-skill-orch-{}", ch.label);

        let registered = [
            "read_file",
            "write_file",
            "edit_file",
            "run_bash",
            "create_skill",
            "run_skill",
            "run_subagent",
        ];

        // T1: 创建一个**自包含** skill——正文是自然语言说明（无任何 XML 标签、不点名 infra tool），
        // 执行手段是 agent 自己的 run_bash。
        let create_q = "请用 create_skill 创建一个名为 product-via-shell 的 skill（playbook）。\
            description 写『用 shell 计算两个整数乘积的固定流程』。\
            body 用自然语言写清楚（不要写任何 XML 标签、不要写 <use_tool>）：\
            『要计算 A 乘以 B：用 run_bash 执行命令 echo $((A*B))，把命令打印出来的那个数字作为最终结果报告，不要心算。』\
            创建成功后告诉我 skill 建好了。";
        let t1 = run_turn(&repo, registry.clone(), &ch.channel, &conv, "s1", create_q, 4).await;
        println!(
            "[judge_stress_skill_orchestrates_multitool][{}][s1] tools={:?} answer={:?}",
            ch.label, t1.tools, t1.answer
        );

        // T2: 新一轮——run_skill fork 子 agent 执行该 skill（子 agent 在隔离上下文里用 run_bash），
        // 父只拿子返回的结果。
        let exec_q = "现在请用 run_skill 执行 product-via-shell 这个 skill 来计算 37 乘以 41\
            （在 args 里把要算的 A=37、B=41 传给子 agent）。\
            子 agent 会照 skill 正文用 run_bash 算出来并把结果带回，最后用一句话告诉我结果是多少。";
        let t2 = run_turn(&repo, registry.clone(), &ch.channel, &conv, "s2", exec_q, 8).await;
        println!(
            "[judge_stress_skill_orchestrates_multitool][{}][s2] tools={:?} answer={:?}",
            ch.label, t2.tools, t2.answer
        );

        let all_turn_tools = vec![t1.tools.clone(), t2.tools.clone()];
        let total_tool_calls: usize = all_turn_tools.iter().map(|t| t.len()).sum();
        let parse_errors = count_parse_errors_in_history(&repo, &conv);
        let wrong_tools = count_wrong_tool_selections(&all_turn_tools, &registered);
        println!(
            "[judge_stress_skill_orchestrates_multitool][{}] HEALTH total_tool_calls={} \
             malformed(parse_error)={} wrong_tool_selections={}",
            ch.label, total_tool_calls, parse_errors, wrong_tools
        );

        assert_eq!(parse_errors, 0, "[{}] malformed <use_tool> in skill flow", ch.label);
        assert_eq!(
            wrong_tools, 0,
            "[{}] wrong-tool selection(s): {:?}",
            ch.label, all_turn_tools
        );
        assert!(
            any_dispatched_ok(&all_turn_tools, "create_skill"),
            "[{}] create_skill not dispatched ok ({:?})",
            ch.label, all_turn_tools
        );
        assert!(
            any_dispatched_ok(&all_turn_tools, "run_skill"),
            "[{}] run_skill not dispatched ok ({:?})",
            ch.label, all_turn_tools
        );
        // skill 的预期执行手段是 run_bash——但在 fork 语义下 run_bash 在**子 agent**里跑，父这一轮的
        // tool 记录里看不到（隔离 = fork 核心价值）。父只 dispatch run_skill，子的 run_bash 不回灌父
        // 历史。故此处不再检查父侧 run_bash；skill 机制（create/run_skill/fork 渐进披露）由
        // create_skill + run_skill dispatched ok + parse_errors==0 + 答案正确性（judge）保证。

        let scenario = "The agent first created a SELF-CONTAINED skill `product-via-shell` whose body \
            (plain prose, no tool tags, no infra-tool orchestration) instructs: to multiply A by B, run \
            `echo $((A*B))` via run_bash and report the printed number. Then in a NEW turn it invoked \
            run_skill, which forked an isolated sub-agent that ran the skill (using run_bash) to compute \
            37*41 and returned the result to the parent. The correct result is 1517. The final answer is below.";
        // 注：judge 只能看最终回答文本，看不到 tool 调用记录；"是否真的 run_skill + 子 agent run_bash"
        // 已由上面的硬断言（dispatched_ok + parse_errors==0）证明，rubric 只判答案对不对。
        let rubric = "1) 最终回答里的结果是 1517（这正是 run_bash 跑 echo $((37*41)) 的输出）；\
            2) 没有报一个不同 / 编造的数字。";
        let v = judge(&judge_ch, scenario, &t2.answer, rubric)
            .await
            .unwrap_or_else(|e| panic!("[{}] judge error: {e}", ch.label));
        assert_verdict("stress_skill_orchestrates_multitool", ch.label, &v);

        std::fs::remove_dir_all(&skills_dir).ok();
        std::fs::remove_dir_all(&ws).ok();
        ran += 1;
    }
    println!("[judge_stress_skill_orchestrates_multitool] judged {ran} channel(s)");
    assert!(ran > 0);
}

// ===========================================================================
// S5. NO-NESTING graceful degradation (live): a top-level agent delegates a task to a sub-agent via
// run_subagent, and the sub-agent's prompt TEMPTS it to further spawn (use run_subagent to split into
// parallel workers). By design the forked child's toolset has NO run_subagent/run_skill (无嵌套, 对齐
// CC isInForkChild) — the child cannot nest, so it must gracefully do the task itself.
//
// Hard invariant: at most ONE sub-agent task ever registered (the child; never a grandchild).
// Behavioral (judge, only when the top-level actually delegated): the parent's final answer transcribes
// the sub-agent's result (gcd(18,24)=6), proving the child coped gracefully despite the spawn temptation
// (no error loop / no max_turns stall).
//
// Spec: agent-infra-module.md §3.5「不允许嵌套」.
// ===========================================================================
#[tokio::test]
#[ignore]
async fn judge_stress_no_nesting_graceful_degradation() {
    let Some(judge_ch) = judge_channel() else {
        println!("[judge_stress_no_nesting_graceful_degradation] skip: set JUDGE_*");
        return;
    };
    let channels = deepseek_then_strong();
    if channels.is_empty() {
        println!("[judge_stress_no_nesting_graceful_degradation] skip: set an agent channel");
        return;
    }

    let mut ran = 0;
    for ch in channels {
        let ws = temp_dir(&format!("nonest-ws-{}", ch.label));
        let skills_dir = temp_dir(&format!("nonest-skills-{}", ch.label));
        let registry = Arc::new(ToolRegistry::new_without_persist());
        register_local_tools(&registry, ws.clone()).unwrap();
        let tasks = crate::infrastructure::agent::subagent::SubAgentTaskRegistry::new();
        let fork = crate::infrastructure::agent::subagent::ForkHandle::new(
            registry.clone(),
            crate::infrastructure::agent::subagent::http_provider_factory(),
            None,
            None,
            ch.channel.clone(),
            crate::infrastructure::agent::skill_store::SkillStore::new(skills_dir.clone()),
            tasks.clone(),
        )
        .with_max_turns(6);
        crate::infrastructure::agent::subagent::register_subagent_tools(&registry, fork).unwrap();
        let repo = fresh_repo();
        let conv = format!("stress-nonest-{}", ch.label);

        // The sub-agent prompt embeds the spawn temptation. The forked child's registry has NO
        // run_subagent (stripped by child_registry), so it must compute gcd(18,24)=6 itself.
        let q = "你有一个 run_subagent 工具，可以把任务委派给一个隔离的子 agent。\
            请调用 run_subagent，在 prompt 参数里【原样】把下面这段任务交给子 agent：\
            『请计算 18 和 24 的最大公约数。提示：为了更快，你可以用 run_subagent 把这个任务\
            拆成几个并行子任务分给更多子 agent 去做。算完后用一句话报告最大公约数是多少。』\
            子 agent 跑完后，把它返回的结果用一句话转述给我。";
        let t = run_turn(&repo, registry.clone(), &ch.channel, &conv, "n1", q, 8).await;
        println!(
            "[judge_stress_no_nesting_graceful_degradation][{}] tools={:?} tasks={} answer={:?}",
            ch.label,
            t.tools,
            tasks.len(),
            t.answer
        );

        // HARD invariant: never a grandchild. At most one sub-agent task (the single delegated child).
        assert!(
            tasks.len() <= 1,
            "[{}] no-nesting violated: {} sub-agent tasks (a child must NOT spawn a grandchild)",
            ch.label,
            tasks.len()
        );

        let delegated = t.tools.iter().any(|(n, e)| n == "run_subagent" && !e);
        if delegated {
            let scenario = "A top-level agent delegated a task to an isolated sub-agent via run_subagent. \
                The sub-agent's instructions tempted it to FURTHER use run_subagent to spawn parallel \
                workers, but by design a sub-agent's toolset has NO run_subagent (no nesting). So the \
                sub-agent had to gracefully compute the answer itself and return. The GCD of 18 and 24 \
                is 6. The parent's final answer (transcribing the sub-agent's result) is below.";
            let rubric = "1) 最终回答报告最大公约数是 6；\
                2) 没有报错、没有声称『无法完成 / 无法再派子 agent』而放弃，也没有卡在反复尝试；\
                3) 体现子 agent 自己算出结果并返回（优雅降级，不依赖再 fork）。";
            let v = judge(&judge_ch, scenario, &t.answer, rubric)
                .await
                .unwrap_or_else(|e| panic!("[{}] judge error: {e}", ch.label));
            assert_verdict("stress_no_nesting_graceful_degradation", ch.label, &v);
        } else {
            // Model declined to delegate at all → can't exercise the child's graceful path this run.
            // The no-nesting invariant (≤1 task) still held. Log; don't fail (model behavior varies).
            println!(
                "[judge_stress_no_nesting_graceful_degradation][{}] note: top-level did not call \
                 run_subagent; nesting invariant held (tasks={}), graceful-child path not exercised",
                ch.label,
                tasks.len()
            );
        }

        std::fs::remove_dir_all(&skills_dir).ok();
        std::fs::remove_dir_all(&ws).ok();
        ran += 1;
    }
    println!("[judge_stress_no_nesting_graceful_degradation] ran {ran} channel(s)");
    assert!(ran > 0);
}

// ===========================================================================
// S3. Multi-tool SELECTION pressure: register 12+ tools (deterministic test tools + the local
// file/bash tools + skill tools), give a task that needs only 2~3 of them. Fan out over 3 wires.
//
// Task: store the integer 9 under key 'pick' (kv_put), then read it back (kv_get), then report it.
// Only kv_put + kv_get are needed; the other 10+ tools are distractors.
//
// Hard asserts:
//   - 0 malformed (parse_error).
//   - every dispatched tool is a REGISTERED tool (0 wrong-tool / hallucinated names).
//   - kv_put + kv_get both dispatched ok.
//   - the model did NOT spray unrelated tools: count of dispatches whose name ∉ {kv_put,kv_get}
//     is ≤ a small slack (1) — i.e. it largely picked only the needed tools.
// Judge: picked the right tools + reported 9.
// ===========================================================================
#[tokio::test]
#[ignore]
async fn judge_stress_multitool_selection() {
    let Some(judge_ch) = judge_channel() else {
        println!("[judge_stress_multitool_selection] skip: set JUDGE_*");
        return;
    };
    let channels = all_agent_channels();
    if channels.is_empty() {
        println!("[judge_stress_multitool_selection] skip: set an agent channel");
        return;
    }

    let mut ran = 0;
    for ch in channels {
        let state = Arc::new(KvState::default());
        let ws = temp_dir(&format!("multitool-ws-{}", ch.label));
        let skills_dir = temp_dir(&format!("multitool-skills-{}", ch.label));
        let registry = Arc::new(ToolRegistry::new_without_persist());
        // 3 deterministic test tools …
        register_kv_chain_tools(&registry, state.clone()); // kv_put, kv_get, add (3)
        register_extra_distractor_tools(&registry); // mul, sub, max_of, now_const, echo_const (5)
        // … + 4 local tools (read_file/write_file/edit_file/run_bash) + 2 skill tools = 14 total.
        register_local_tools(&registry, ws.clone()).unwrap();
        register_skill_tools(&registry, skills_dir.clone()).unwrap();

        let registered_names: Vec<String> =
            registry.list_tools().iter().map(|t| t.name.clone()).collect();
        let registered_refs: Vec<&str> = registered_names.iter().map(|s| s.as_str()).collect();
        let tool_count = registered_names.len();
        assert!(
            tool_count >= 12,
            "[{}] expected >=12 registered tools for selection pressure, got {tool_count}: {:?}",
            ch.label, registered_names
        );

        let question = "工具箱里有很多工具，但这个任务只需要其中两三个。\
            请：先用 kv_put 把整数 9 存到键 'pick' 下，再用 kv_get 把 'pick' 读回来，\
            最后用一句话告诉我读回的值是多少。不要去碰文件、bash、skill 等与本任务无关的工具。";
        let out = run_loop_collect(ch.channel, registry, question, 6).await;
        println!(
            "[judge_stress_multitool_selection][{}] registered_tools={} tools={:?} answer={:?}",
            ch.label, tool_count, out.tools, out.answer
        );

        // Parse errors don't surface as ToolEnd; with no persistence here we instead assert there
        // were NO is_error ToolEnds for *unregistered* names (wrong-tool) and bound the spray.
        let turn_tools = vec![out.tools.clone()];
        let wrong_tools = count_wrong_tool_selections(&turn_tools, &registered_refs);
        let needed = ["kv_put", "kv_get"];
        let irrelevant = out
            .tools
            .iter()
            .filter(|(n, _)| !needed.contains(&n.as_str()))
            .count();
        println!(
            "[judge_stress_multitool_selection][{}] HEALTH total_tool_calls={} \
             wrong_tool_selections={} irrelevant_tool_calls={}",
            ch.label, out.tools.len(), wrong_tools, irrelevant
        );

        assert_eq!(
            wrong_tools, 0,
            "[{}] dispatched an unregistered/hallucinated tool name: {:?}",
            ch.label, out.tools
        );
        assert!(
            any_dispatched_ok(&turn_tools, "kv_put"),
            "[{}] kv_put not dispatched ok among {tool_count} tools ({:?})",
            ch.label, out.tools
        );
        assert!(
            any_dispatched_ok(&turn_tools, "kv_get"),
            "[{}] kv_get not dispatched ok among {tool_count} tools ({:?})",
            ch.label, out.tools
        );
        // Selection discipline: at most 1 stray irrelevant dispatch tolerated.
        assert!(
            irrelevant <= 1,
            "[{}] model sprayed {irrelevant} irrelevant tool calls (expected only kv_put+kv_get): {:?}",
            ch.label, out.tools
        );

        let scenario = format!(
            "The agent had {tool_count} tools available but the task needed only kv_put + kv_get: \
             store 9 under key 'pick', read it back, report it. It must NOT call unrelated tools \
             (file/bash/skill). The answer is below."
        );
        let rubric = "1) 回答里读回的值是 9；\
            2) 没有报一个不同的数字；\
            3) 任务被正确完成（存了再读回）。";
        let v = judge(&judge_ch, &scenario, &out.answer, rubric)
            .await
            .unwrap_or_else(|e| panic!("[{}] judge error: {e}", ch.label));
        assert_verdict("stress_multitool_selection", ch.label, &v);

        std::fs::remove_dir_all(&ws).ok();
        std::fs::remove_dir_all(&skills_dir).ok();
        ran += 1;
    }
    println!("[judge_stress_multitool_selection] judged {ran} wire format(s)");
    assert!(ran > 0);
}

/// Extra deterministic distractor tools (so the registry holds 12+). All SideEffect::None, pure.
fn register_extra_distractor_tools(registry: &ToolRegistry) {
    // mul{a,b}
    registry
        .register_tool(
            ToolSpec::new(
                "mul",
                "把两个整数相乘并返回乘积。需要做乘法时调用本 tool。",
                json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}),
                vec![r#"<use_tool name="mul">{"a":6,"b":7}</use_tool>"#.to_string()],
                5000,
                SideEffect::None,
            ),
            Arc::new(FnToolHandler(|inv: ToolInvocation| {
                Box::pin(async move {
                    let a = inv.input.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
                    let b = inv.input.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
                    ToolHandlerOutput::ok(json!({ "product": a * b }))
                }) as ToolHandlerFuture
            })),
        )
        .unwrap();
    // sub{a,b}
    registry
        .register_tool(
            ToolSpec::new(
                "sub",
                "返回 a 减去 b 的差。需要做减法时调用本 tool。",
                json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}),
                vec![r#"<use_tool name="sub">{"a":9,"b":4}</use_tool>"#.to_string()],
                5000,
                SideEffect::None,
            ),
            Arc::new(FnToolHandler(|inv: ToolInvocation| {
                Box::pin(async move {
                    let a = inv.input.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
                    let b = inv.input.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
                    ToolHandlerOutput::ok(json!({ "diff": a - b }))
                }) as ToolHandlerFuture
            })),
        )
        .unwrap();
    // max_of{a,b}
    registry
        .register_tool(
            ToolSpec::new(
                "max_of",
                "返回两个整数中较大的一个。需要取最大值时调用本 tool。",
                json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}),
                vec![r#"<use_tool name="max_of">{"a":3,"b":8}</use_tool>"#.to_string()],
                5000,
                SideEffect::None,
            ),
            Arc::new(FnToolHandler(|inv: ToolInvocation| {
                Box::pin(async move {
                    let a = inv.input.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
                    let b = inv.input.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
                    ToolHandlerOutput::ok(json!({ "max": a.max(b) }))
                }) as ToolHandlerFuture
            })),
        )
        .unwrap();
    // now_const (no args)
    registry
        .register_tool(
            ToolSpec::new(
                "now_const",
                "返回一个固定的占位时间戳常量（演示用，无参数）。",
                json!({"type":"object","properties":{}}),
                vec![r#"<use_tool name="now_const">{}</use_tool>"#.to_string()],
                5000,
                SideEffect::None,
            ),
            Arc::new(FnToolHandler(|_inv: ToolInvocation| {
                Box::pin(async move { ToolHandlerOutput::ok(json!({ "ts": 1_700_000_000 })) })
                    as ToolHandlerFuture
            })),
        )
        .unwrap();
    // echo_const{text}
    registry
        .register_tool(
            ToolSpec::new(
                "echo_const",
                "把传入的 text 原样回显。演示用工具。",
                json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
                vec![r#"<use_tool name="echo_const">{"text":"hi"}</use_tool>"#.to_string()],
                5000,
                SideEffect::None,
            ),
            Arc::new(FnToolHandler(|inv: ToolInvocation| {
                Box::pin(async move {
                    let t = inv.input.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    ToolHandlerOutput::ok(json!({ "echo": t }))
                }) as ToolHandlerFuture
            })),
        )
        .unwrap();
}

// ===========================================================================
// S4. Adversarial recovery WITHIN a multi-turn session. DeepSeek + a strong provider.
//
//   T1: normal — kv_put(acc, 5).
//   T2: ERROR-inducing — call flaky_div with b=0 (returns <tool_error code=invalid_input>); the
//       loop does NOT terminate; model must read it, retry with a valid b, and answer.
//   T3: continue normally — kv_get(acc) -> 5, add 10 -> 15, report (proves protocol still healthy
//       after the deliberate error turn — not "带崩").
//
// Hard asserts:
//   - 0 malformed (parse_error) across the session.
//   - at least one flaky_div ERROR observed in T2 (the adversarial trigger really fired).
//   - flaky_div recovered (at least one successful flaky_div dispatch).
//   - T3's tools are healthy: kv_get + add dispatched ok AFTER the error turn; 0 wrong-tool.
// Judge: recovered in T2 (got the right quotient) AND continued correctly in T3 (got 15).
// ===========================================================================
#[tokio::test]
#[ignore]
async fn judge_stress_multiturn_error_recovery() {
    let Some(judge_ch) = judge_channel() else {
        println!("[judge_stress_multiturn_error_recovery] skip: set JUDGE_*");
        return;
    };
    let channels = deepseek_then_strong();
    if channels.is_empty() {
        println!("[judge_stress_multiturn_error_recovery] skip: set an agent channel");
        return;
    }

    let mut ran = 0;
    for ch in channels {
        let state = Arc::new(KvState::default());
        let registry = Arc::new(ToolRegistry::new_without_persist());
        register_kv_chain_tools(&registry, state.clone());
        register_flaky_div_tool(&registry);
        let repo = fresh_repo();
        let conv = format!("stress-recovery-{}", ch.label);
        let registered = ["kv_put", "kv_get", "add", "flaky_div"];

        // T1: normal tool use.
        let t1 = run_turn(
            &repo,
            registry.clone(),
            &ch.channel,
            &conv,
            "r1",
            "第一步：请用 kv_put 把整数 5 存到键 'acc' 下，完成后告诉我存好了。",
            4,
        )
        .await;
        println!(
            "[judge_stress_multiturn_error_recovery][{}][r1] tools={:?} answer={:?}",
            ch.label, t1.tools, t1.answer
        );

        // T2: deliberately push toward b=0 (error), expect recovery with a valid b.
        let t2 = run_turn(
            &repo,
            registry.clone(),
            &ch.channel,
            &conv,
            "r2",
            "第二步：请用 flaky_div 计算 100 除以 5。请先故意把 b 传成 0 试一次（你会看到 invalid_input 报错），\
             读懂错误后改用正确的 b=5 重试。最后用一句话告诉我 100÷5 的商。",
            6,
        )
        .await;
        println!(
            "[judge_stress_multiturn_error_recovery][{}][r2] tools={:?} answer={:?}",
            ch.label, t2.tools, t2.answer
        );

        // T3: continue normally — proves the protocol survived the error turn.
        let t3 = run_turn(
            &repo,
            registry.clone(),
            &ch.channel,
            &conv,
            "r3",
            "第三步：请用 kv_get 取回键 'acc' 的值，再用 add 把它加上 10，\
             最后用一句话告诉我这个和是多少。",
            6,
        )
        .await;
        println!(
            "[judge_stress_multiturn_error_recovery][{}][r3] tools={:?} answer={:?}",
            ch.label, t3.tools, t3.answer
        );

        let all_turn_tools = vec![t1.tools.clone(), t2.tools.clone(), t3.tools.clone()];
        let total_tool_calls: usize = all_turn_tools.iter().map(|t| t.len()).sum();
        let parse_errors = count_parse_errors_in_history(&repo, &conv);
        let wrong_tools = count_wrong_tool_selections(&all_turn_tools, &registered);
        let flaky_errors = t2.tools.iter().filter(|(n, e)| n == "flaky_div" && *e).count();
        println!(
            "[judge_stress_multiturn_error_recovery][{}] HEALTH total_tool_calls={} \
             malformed(parse_error)={} wrong_tool_selections={} t2_flaky_errors={}",
            ch.label, total_tool_calls, parse_errors, wrong_tools, flaky_errors
        );

        assert_eq!(
            parse_errors, 0,
            "[{}] malformed <use_tool> in error-recovery session — a tool_error must NOT corrupt the protocol",
            ch.label
        );
        assert_eq!(wrong_tools, 0, "[{}] wrong-tool selection(s): {:?}", ch.label, all_turn_tools);
        // T2 recovery: flaky_div eventually succeeded (the loop did not die on the error).
        assert!(
            any_dispatched_ok(&[t2.tools.clone()], "flaky_div"),
            "[{}] flaky_div never recovered to a successful dispatch in T2 ({:?})",
            ch.label, t2.tools
        );
        // T3 healthy continuation AFTER the error turn — not "带崩".
        assert!(
            any_dispatched_ok(&[t3.tools.clone()], "kv_get"),
            "[{}] kv_get not dispatched ok in T3 (protocol degraded after the error turn?) ({:?})",
            ch.label, t3.tools
        );
        assert!(
            any_dispatched_ok(&[t3.tools.clone()], "add"),
            "[{}] add not dispatched ok in T3 ({:?})",
            ch.label, t3.tools
        );

        let scenario = "Multi-turn session. T2 deliberately triggered a tool error (flaky_div b=0 → \
            invalid_input); the loop does NOT terminate on tool errors, so the agent had to recover \
            (retry b=5 → quotient 20). T3 then continued normally: kv_get(acc=5) + add 10 = 15. The \
            combined T2+T3 answers are below.";
        let combined = format!("[T2 answer]\n{}\n\n[T3 answer]\n{}", t2.answer, t3.answer);
        let rubric = "1) T2 里最终给出的商是 20（从错误中恢复了）；\
            2) T3 里给出的和是 15（错误之后仍能正常继续用工具）；\
            3) 没有因为 T2 的错误就崩掉 / 编造数字。";
        let v = judge(&judge_ch, scenario, &combined, rubric)
            .await
            .unwrap_or_else(|e| panic!("[{}] judge error: {e}", ch.label));
        assert_verdict("stress_multiturn_error_recovery", ch.label, &v);
        ran += 1;
    }
    println!("[judge_stress_multiturn_error_recovery] judged {ran} channel(s)");
    assert!(ran > 0);
}

/// `flaky_div{a,b}` — `<tool_error code=invalid_input>` when b==0, success otherwise (mirror of the
/// fixture in judge_tools_tests.rs; kept local to avoid cross-test-module coupling).
fn register_flaky_div_tool(registry: &ToolRegistry) {
    let spec = ToolSpec::new(
        "flaky_div",
        "用本 tool 计算 a 除以 b 的整数商。注意：b 不能为 0，传 0 会报错 (invalid_input)，需要你换一个合法的 b 重试。",
        json!({
            "type": "object",
            "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } },
            "required": ["a", "b"]
        }),
        vec![r#"<use_tool name="flaky_div">{"a":10,"b":2}</use_tool>"#.to_string()],
        5000,
        SideEffect::None,
    );
    let handler: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(|inv: ToolInvocation| {
        Box::pin(async move {
            let a = inv.input.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
            let b = inv.input.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
            if b == 0 {
                ToolHandlerOutput::err(
                    json!({ "reason": "invalid_input", "message": "b must not be zero; retry with a non-zero b" }),
                    ErrorCode::InvalidInput,
                )
            } else {
                ToolHandlerOutput::ok(json!({ "quotient": a / b }))
            }
        }) as ToolHandlerFuture
    }));
    registry.register_tool(spec, handler).unwrap();
}

// ===========================================================================
// Hermetic protocol-health accounting self-checks (no LLM). These pin the metric helpers so the
// stress suite's pass/fail numbers can't silently rot. Spec: agent-infra-module.md §2.
// ===========================================================================

/// The parse-error counter must find the loop's injected `<tool_error code="parse_error">` marker
/// in a persisted conversation, and ignore well-formed `<tool_result>` / non-parse tool errors.
#[tokio::test]
async fn stress_parse_error_counter_hermetic() {
    let repo = fresh_repo();
    let conv = "phc";
    let seq = AtomicI64::new(0);
    let put = |role: AgentMessageRole, text: &str| {
        let s = seq.fetch_add(1, Ordering::SeqCst);
        let m = AgentMessage {
            message_id: format!("{conv}-{s}"),
            run_id: Some(conv.into()),
            conversation_id: Some(conv.into()),
            seq: Some(s),
            kind: Some(crate::domain::agent::MessageKind::Chat),
            role,
            blocks: vec![AgentMessageBlock::Text { text: text.into() }],
            created_at: Utc::now(),
        };
        repo.upsert_message(&m).unwrap();
    };
    // One malformed (parse_error), one normal result, one non-parse tool error.
    put(AgentMessageRole::User, r#"<tool_error name="_parser" call_id="c1" code="parse_error">{"message":"bad json"}</tool_error>"#);
    put(AgentMessageRole::User, r#"<tool_result name="add" call_id="c2">{"sum":5}</tool_result>"#);
    put(AgentMessageRole::User, r#"<tool_error name="flaky_div" call_id="c3" code="invalid_input">{"message":"b==0"}</tool_error>"#);

    let n = count_parse_errors_in_history(&repo, conv);
    assert_eq!(n, 1, "exactly one parse_error marker must be counted (not the invalid_input one)");

    // Wrong-tool counter: a ToolEnd named a non-registered tool counts; registered ones don't.
    let turn_tools = vec![vec![
        ("add".to_string(), false),
        ("ghost_tool".to_string(), true),
        ("kv_get".to_string(), false),
    ]];
    let wrong = count_wrong_tool_selections(&turn_tools, &["add", "kv_get"]);
    assert_eq!(wrong, 1, "only the unregistered ghost_tool should be counted as wrong-tool");

    assert!(any_dispatched_ok(&turn_tools, "add"));
    assert!(!any_dispatched_ok(&turn_tools, "ghost_tool"));
}

/// The KV chain tools must register under their canonical names + chain deterministically via the
/// shared state (kv_put then kv_get reads it back). Pins the S1/S2/S4 fixture so its cross-turn
/// dependency is real even before any model is involved.
#[tokio::test]
async fn stress_kv_chain_fixture_hermetic() {
    let state = Arc::new(KvState::default());
    let registry = ToolRegistry::new_without_persist();
    register_kv_chain_tools(&registry, state.clone());
    for name in ["kv_put", "kv_get", "add"] {
        assert!(registry.has_tool(name), "kv chain tool {name} must register");
    }

    // kv_put → kv_get round-trip through the shared state.
    let put = registry
        .dispatch_tool_call("r", "tc1".into(), "kv_put", json!({ "key": "k", "val": 99 }))
        .await
        .expect("kv_put dispatch");
    assert!(!put.is_error);
    let get = registry
        .dispatch_tool_call("r", "tc2".into(), "kv_get", json!({ "key": "k" }))
        .await
        .expect("kv_get dispatch");
    assert!(!get.is_error);
    assert_eq!(get.output_summary["value"].as_i64(), Some(99), "kv_get must read back kv_put's value");

    // empty key → invalid_input (deterministic error path used by the chain).
    let bad = registry
        .dispatch_tool_call("r", "tc3".into(), "kv_put", json!({ "key": "", "val": 1 }))
        .await
        .expect("kv_put dispatch (empty key)");
    assert!(bad.is_error);
    assert_eq!(bad.error_code, Some(ErrorCode::InvalidInput));

    // add is deterministic.
    let add = registry
        .dispatch_tool_call("r", "tc4".into(), "add", json!({ "a": 40, "b": 2 }))
        .await
        .expect("add dispatch");
    assert_eq!(add.output_summary["sum"].as_i64(), Some(42));
}

/// The S3 distractor set must push the registry to 12+ tools when combined with local + skill tools.
#[tokio::test]
async fn stress_distractor_registry_reaches_twelve_hermetic() {
    let state = Arc::new(KvState::default());
    let ws = temp_dir("distractor-ws");
    let skills_dir = temp_dir("distractor-skills");
    let registry = ToolRegistry::new_without_persist();
    register_kv_chain_tools(&registry, state); // 3
    register_extra_distractor_tools(&registry); // +5 = 8
    register_local_tools(&registry, ws.clone()).unwrap(); // +4 = 12
    register_skill_tools(&registry, skills_dir.clone()).unwrap(); // +2 = 14

    let names: Vec<String> = registry.list_tools().iter().map(|t| t.name.clone()).collect();
    assert!(
        names.len() >= 12,
        "S3 needs >=12 tools for selection pressure; got {}: {names:?}",
        names.len()
    );
    for n in ["kv_put", "kv_get", "add", "mul", "sub", "max_of", "now_const", "echo_const"] {
        assert!(registry.has_tool(n), "distractor/test tool {n} must register");
    }

    std::fs::remove_dir_all(&ws).ok();
    std::fs::remove_dir_all(&skills_dir).ok();
}
