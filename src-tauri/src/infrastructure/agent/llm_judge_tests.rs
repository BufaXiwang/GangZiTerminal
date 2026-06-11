//! LLM-as-Judge live test suite for Agent Infra.
//!
//! Spec: docs/design/agent-infra-module.md §2 (Tool 协议 / durable facts) /
//!       §3 (Agent Loop) / §4 (上下文管理 / Summarize / compaction) / §5 (Infra Loop API).
//!
//! These tests are NOT deterministic script assertions. Each one drives a *real* agent-infra
//! capability live (run_agent_turn over a real HttpProvider), then asks a
//! separate JUDGE LLM to evaluate the produced output against a rubric, returning a structured
//! `{pass, score, reason}` verdict. The judge semantically validates the agent's core behaviors:
//! answer relevance, tool/tool faithfulness, multi-turn memory, summary faithfulness, and
//! durable-fact preservation through context compaction.
//!
//! ALL tests are `#[ignore]` (live, env-creds) — they never run in a normal `cargo test`.
//!
//! Channels / env (TEST-ONLY env, never hardcode secrets):
//!   Agent-under-test channel (fast, e.g. DeepSeek):
//!     TEST_DS_BASE / TEST_DS_KEY / TEST_DS_MODEL   (chat_completions)
//!     TEST_ANT_BASE / TEST_ANT_KEY / TEST_ANT_MODEL (messages)
//!     TEST_OAI_BASE / TEST_OAI_KEY / TEST_OAI_MODEL (responses)
//!   Judge channel (separate, strong evaluator):
//!     JUDGE_BASE / JUDGE_KEY / JUDGE_MODEL / JUDGE_WIRE (default: messages)
//!
//! Each test skips (prints) when its required env is unset, mirroring the existing live tests.
//!
//! Run the whole suite (example env line; creds shown are placeholders):
//!   TEST_DS_BASE=https://api.deepseek.com TEST_DS_KEY=... TEST_DS_MODEL=deepseek-v4-flash \
//!   JUDGE_BASE=https://.../api JUDGE_KEY=... JUDGE_MODEL=claude-opus-4-5-20251101 JUDGE_WIRE=messages \
//!   cargo test --manifest-path src-tauri/Cargo.toml judge_ -- --ignored --nocapture

use std::sync::Arc;

use chrono::Utc;
use serde_json::json;
use tokio::sync::mpsc;

use crate::domain::agent::{
    AgentEvent, AgentMessage, AgentMessageBlock, AgentMessageRole, AgentRunRequest, AgentStopReason,
    CompactionConfig, ContextBundle, MessageKind, ProviderChannel, SideEffect, ToolSpec,
    WireFormat,
};
use crate::infrastructure::agent::http_provider::HttpProvider;
use crate::infrastructure::agent::loop_executor::{run_agent_turn, ProviderStream};
use crate::infrastructure::agent::messages_repo::AgentMessagesRepo;
use crate::infrastructure::agent::tool_registry::{
    FnToolHandler, ToolHandler, ToolHandlerFuture, ToolHandlerOutput, ToolInvocation,
    ToolRegistry,
};

// ---------------------------------------------------------------------------
// Channel helpers (mirror the existing live tests' env wiring)
// ---------------------------------------------------------------------------

/// Base channel template (no creds). Same shape as loop_executor::tests::channel().
fn base_channel() -> ProviderChannel {
    ProviderChannel {
        channel_id: "judge-suite".into(),
        provider: "judge-suite".into(),
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

/// A resolved agent-under-test channel selected from env.
struct AgentChannel {
    label: &'static str,
    channel: ProviderChannel,
}

/// Resolve the responses (OpenAI) channel from env, if present.
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

/// Resolve the messages (Anthropic) channel from env, if present.
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

/// Resolve the chat_completions (DeepSeek / OpenAI-compatible) channel from env, if present.
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

/// Pick the fastest available agent-under-test channel (prefer chat_completions, then messages).
/// Used by the multi-turn / compaction tests that don't need to fan out across all wires.
fn pick_fast_agent_channel() -> Option<AgentChannel> {
    agent_channel_chat_completions()
        .or_else(agent_channel_messages)
        .or_else(agent_channel_responses)
}

// ---------------------------------------------------------------------------
// Judge harness
// ---------------------------------------------------------------------------

/// A judge verdict parsed from the judge LLM's JSON reply.
#[derive(Debug, Clone)]
struct Verdict {
    pass: bool,
    score: f32,
    reason: String,
}

/// Build the JUDGE channel from JUDGE_* env. Returns None (caller skips) if unset.
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
    // Judges only need a short JSON verdict, but give headroom for reasoning models.
    c.max_output_tokens = Some(1024);
    Some(c)
}

/// The strict-evaluator system instruction. The judge must reply ONLY with a JSON object.
const JUDGE_SYSTEM: &str = "You are a STRICT evaluator of an AI agent's output. \
You are given a scenario, the agent's output, and a rubric. \
Decide whether the agent output satisfies ALL rubric criteria. \
You MUST respond with ONLY a single raw JSON object and NOTHING else — no reasoning, no analysis, \
no markdown, no prose before or after. Your entire reply must be exactly one JSON object in this shape: \
{\"pass\": <true|false>, \"score\": <number between 0 and 1>, \"reason\": \"<short explanation, max 200 chars>\"}. \
Put all of your reasoning into the \"reason\" field; do NOT write any reasoning outside the JSON. \
Set \"pass\" to true ONLY if every rubric criterion is satisfied. \
Begin your reply with the character { and end it with the character }.";

/// Assistant prefill — forcing the judge's reply to start with `{` so reasoning-prone models can't
/// emit a prose preamble (Anthropic Messages / OpenAI honor a leading assistant turn as a prefix
/// the model continues from). Combined with JUDGE_SYSTEM this reliably yields a parseable object.
const JUDGE_PREFILL: &str = "{\"pass\":";

/// Run one collected (non-streaming-collected) judge call over the judge channel and parse a
/// `Verdict`. On any parse failure, returns the raw reply text inside `Err` so the test can fail
/// loudly with the raw output.
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
        // Assistant prefill: forces the reply to continue from `{"pass":` (no prose preamble).
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
    // Throwaway event sink: we don't need the judge's stream deltas, only the collected text.
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

    // With a prefill, the API returns only the *continuation* (prefill not echoed). Try the raw
    // reply first; if it isn't parseable on its own, stitch the prefill back on and retry.
    if let Some(v) = parse_verdict(&raw) {
        return Ok(v);
    }
    let stitched = format!("{JUDGE_PREFILL}{raw}");
    parse_verdict(&stitched)
        .ok_or_else(|| format!("judge reply not parseable as verdict JSON: {raw:?}"))
}

/// Robustly extract the `{...}` JSON object from a judge reply (the model may wrap it in prose or
/// markdown fences) and parse it into a `Verdict`. Returns None on failure.
fn parse_verdict(raw: &str) -> Option<Verdict> {
    let obj = extract_json_object(raw)?;
    let v: serde_json::Value = serde_json::from_str(&obj).ok()?;
    let pass = v.get("pass").and_then(|x| x.as_bool())?;
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
    Some(Verdict { pass, score, reason })
}

/// Find the first balanced `{ ... }` JSON object substring (handles strings + escapes so braces
/// inside string literals don't confuse the matcher).
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

/// Assert a verdict passes; always print score + reason so a human sees WHY.
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

// ---------------------------------------------------------------------------
// Run-loop helper: drive run_agent_turn and collect the streamed answer + tools
// ---------------------------------------------------------------------------

struct LoopRun {
    answer: String,
    tools: Vec<(String, bool)>,
    stop_reason: AgentStopReason,
}

/// Run a single-shot loop (one user question) over the given channel + registry; pump the event
/// stream and collect the streamed TextDelta answer + tool_end (name, is_error) pairs.
async fn run_loop_collect(
    channel: ProviderChannel,
    registry: Arc<ToolRegistry>,
    user_text: &str,
    max_turns: u32,
) -> LoopRun {
    let provider = Box::new(HttpProvider::new(channel.clone()).unwrap());
    let request = AgentRunRequest {
        run_id: "judge-run".into(),
        trigger: "user".into(),
        channel,
        max_turns,
        input: vec![user_message("judge-run", user_text)],
        conversation_id: None,
        compaction: None,
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
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
    let summary = run_agent_turn(request, registry, ContextBundle::new("judge-run"), vec![provider], tx, None)
        .await
        .expect("loop run");
    let (answer, tools) = pump.await.unwrap();
    LoopRun { answer, tools, stop_reason: summary.stop_reason }
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

/// Fresh in-memory AppDb-backed messages repo (mirrors loop_executor::tests::fresh_repo).
fn fresh_repo() -> AgentMessagesRepo {
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::db::{run_migrations, AppDb};
    let db = AppDb::open_in_memory().unwrap();
    db.with(|c| run_migrations(c, agent_migrations()).unwrap());
    AgentMessagesRepo::new(db)
}

// ===========================================================================
// 1. answer_relevance — for each wire format present, ask a concrete A股 question,
//    run run_agent_turn, judge on-topic / plausible / Chinese / addresses the question.
// ===========================================================================
#[tokio::test]
#[ignore]
async fn judge_answer_relevance() {
    let Some(judge_ch) = judge_channel() else {
        println!("[judge_answer_relevance] skip: set JUDGE_BASE/JUDGE_KEY/JUDGE_MODEL");
        return;
    };

    let question = "请用中文解释A股的T+1交易制度是什么，并说明它对当日买入的股票有什么限制。";
    let rubric = "1) 回答使用中文；\
        2) 回答切题，确实在解释 A股 T+1 交易制度；\
        3) 至少正确指出『当日买入的股票当日不能卖出（要到下一交易日才能卖）』这一核心限制；\
        4) 内容事实上可信，没有明显胡编。";

    let mut ran = 0;
    for ch in [
        agent_channel_responses(),
        agent_channel_messages(),
        agent_channel_chat_completions(),
    ]
    .into_iter()
    .flatten()
    {
        let registry = Arc::new(ToolRegistry::new_without_persist());
        let run = run_loop_collect(ch.channel, registry, question, 1).await;
        println!("[judge_answer_relevance][{}] answer={:?}", ch.label, run.answer);
        assert!(!run.answer.is_empty(), "[{}] empty answer", ch.label);
        let v = judge(&judge_ch, question, &run.answer, rubric)
            .await
            .unwrap_or_else(|e| panic!("[{}] judge error: {e}", ch.label));
        assert_verdict("answer_relevance", ch.label, &v);
        ran += 1;
    }
    println!("[judge_answer_relevance] judged {ran} wire format(s)");
    assert!(ran > 0, "no agent-under-test channel env provided (TEST_OAI_*/TEST_ANT_*/TEST_DS_*)");
}

// ===========================================================================
// 2. tool_faithfulness — register get_quote returning a FIXED made-up price (1234.5);
//    prompt the model to use it + report the price; judge that the model reports 1234.5
//    from the tool and did NOT invent a different number.
// ===========================================================================
#[tokio::test]
#[ignore]
async fn judge_tool_faithfulness() {
    let Some(judge_ch) = judge_channel() else {
        println!("[judge_tool_faithfulness] skip: set JUDGE_*");
        return;
    };
    let Some(ch) = pick_fast_agent_channel() else {
        println!("[judge_tool_faithfulness] skip: set an agent channel (TEST_DS_*/TEST_ANT_*/TEST_OAI_*)");
        return;
    };

    let registry = Arc::new(ToolRegistry::new_without_persist());
    let spec = ToolSpec::new(
        "get_quote",
        "获取单只 A股标的的实时行情快照。需要报某标的现价时必须调用本 tool 获取，不要凭空编造价格。",
        json!({"type":"object","properties":{"tsCode":{"type":"string"}},"required":["tsCode"]}),
        vec![r#"<use_tool name="get_quote">{"tsCode":"600519.SH"}</use_tool>"#.to_string()],
        5000,
        SideEffect::None,
    );
    // FIXED made-up price — the only correct number the model can report.
    let handler: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(|_inv: ToolInvocation| {
        Box::pin(async move { ToolHandlerOutput::ok(json!({"price": "1234.5"})) }) as ToolHandlerFuture
    }));
    registry.register_tool(spec, handler).unwrap();

    let question = "请调用 get_quote 获取 600519.SH 的现价，然后用一句话告诉我它现在多少钱。";
    let run = run_loop_collect(ch.channel, registry, question, 4).await;
    println!(
        "[judge_tool_faithfulness][{}] stop={:?} tools={:?} answer={:?}",
        ch.label, run.stop_reason, run.tools, run.answer
    );
    assert!(
        run.tools.iter().any(|(n, err)| n == "get_quote" && !err),
        "[{}] get_quote was not dispatched successfully",
        ch.label
    );

    let scenario = "An A-share agent must report a stock price. The ONLY authoritative price came \
        from a tool (get_quote) which returned exactly 1234.5. The agent must not invent a different number.";
    let rubric = "1) 回答里报出的价格就是工具返回的 1234.5（可带单位/币种，如 1234.5 元）；\
        2) 没有编造一个不同的数字当作现价；\
        3) 回答使用中文。";
    let v = judge(&judge_ch, scenario, &run.answer, rubric)
        .await
        .unwrap_or_else(|e| panic!("judge error: {e}"));
    assert_verdict("tool_faithfulness", ch.label, &v);
}

// ===========================================================================
// 3. multiturn_memory — turn1 states a constraint only the user knows
//    (只买银行股); turn2 (continued via run_agent_turn + conversation_id + persistence)
//    asks for a recommendation direction; judge that turn-2 respects the constraint.
// ===========================================================================
#[tokio::test]
#[ignore]
async fn judge_multiturn_memory() {
    let Some(judge_ch) = judge_channel() else {
        println!("[judge_multiturn_memory] skip: set JUDGE_*");
        return;
    };
    let Some(ch) = pick_fast_agent_channel() else {
        println!("[judge_multiturn_memory] skip: set an agent channel");
        return;
    };

    let repo = fresh_repo();
    let registry = Arc::new(ToolRegistry::new_without_persist());
    let conversation_id = "judge-mem-conv".to_string();

    // Turn 1: user states the constraint. New contract: hand only the new user message; the
    // engine persists it + its produced assistant reply under conversation_id.
    let provider1 = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req1 = AgentRunRequest {
        run_id: "mem-1".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 1,
        input: vec![user_message("mem-1", "我的风险偏好是只买银行股，请记住这一点。")],
        conversation_id: Some(conversation_id.clone()),
        compaction: None,
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
    };
    let (tx1, mut rx1) = mpsc::channel::<AgentEvent>(256);
    let pump1 = tokio::spawn(async move { while rx1.recv().await.is_some() {} });
    let _ = run_agent_turn(
        req1,
        registry.clone(),
        ContextBundle::new("mem-1"),
        vec![provider1],
        tx1,
        Some(repo.clone()),
    )
    .await
    .expect("turn 1");
    pump1.await.unwrap();

    // Turn 2: NEW run, same conversation_id. Hand only the new user message → the engine persists
    // it and auto-loads the compressed view (turn-1 history) as the turn context.
    let provider2 = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req2 = AgentRunRequest {
        run_id: "mem-2".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 1,
        input: vec![user_message("mem-2", "根据我之前告诉你的偏好，给我推荐一个值得关注的方向。")],
        conversation_id: Some(conversation_id.clone()),
        compaction: None,
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
    };
    let (tx2, mut rx2) = mpsc::channel::<AgentEvent>(256);
    let pump2 = tokio::spawn(async move {
        let mut text = String::new();
        while let Some(e) = rx2.recv().await {
            if let AgentEvent::TextDelta { delta, .. } = e {
                text.push_str(&delta);
            }
        }
        text
    });
    let _ = run_agent_turn(
        req2,
        registry,
        ContextBundle::new("mem-2"),
        vec![provider2],
        tx2,
        Some(repo.clone()),
    )
    .await
    .expect("turn 2");
    let turn2_answer = pump2.await.unwrap();
    println!("[judge_multiturn_memory][{}] turn2_answer={:?}", ch.label, turn2_answer);
    assert!(!turn2_answer.is_empty(), "empty turn-2 answer");

    let scenario = "Multi-turn conversation. In turn 1 the user said their risk preference is to \
        buy ONLY bank stocks (只买银行股). In turn 2 they asked for a recommendation 'based on my \
        preference'. The turn-2 answer below must respect the turn-1 constraint.";
    let rubric = "1) turn-2 的回答尊重 turn-1 的约束，只围绕银行股/银行板块给方向；\
        2) 没有推荐银行股以外的行业/板块作为建议方向；\
        3) 表明模型确实记住了『只买银行股』这个偏好。";
    let v = judge(&judge_ch, scenario, &turn2_answer, rubric)
        .await
        .unwrap_or_else(|e| panic!("judge error: {e}"));
    assert_verdict("multiturn_memory", ch.label, &v);
}

// ===========================================================================
// 4. summary_faithfulness — build a multi-message conversation with concrete facts, force
//    Summarize (tiny summarize_threshold + summarizePrompt covering required slots); judge the
//    produced Summary message text against the original conversation.
// ===========================================================================
#[tokio::test]
#[ignore]
async fn judge_summary_faithfulness() {
    let Some(judge_ch) = judge_channel() else {
        println!("[judge_summary_faithfulness] skip: set JUDGE_*");
        return;
    };
    let Some(ch) = pick_fast_agent_channel() else {
        println!("[judge_summary_faithfulness] skip: set an agent channel");
        return;
    };

    let repo = fresh_repo();
    let registry = Arc::new(ToolRegistry::new_without_persist());
    let conversation_id = "judge-sum-conv".to_string();

    // Concrete facts seeded as a multi-message conversation.
    let original_facts = "\
user: 我关注两只标的：贵州茅台(600519.SH)和招商银行(600036.SH)。\n\
assistant: 好的，已记下贵州茅台与招商银行两只关注标的。\n\
user: 我已经建立的判断是：茅台估值偏高暂时观望，招行可以逢低分批买入。\n\
assistant: 明白。茅台观望、招行逢低分批，是你已建立的判断。\n\
user: 我的风险纪律是单只个股仓位不超过总仓位的20%，且不做融资融券。\n\
assistant: 收到，单票上限20%、不碰两融，是你的风险纪律。\n\
user: 还有一个未决问题：招行的分批买入点位我还没想清楚，需要再观察。";

    let mk = |seq: i64, role: AgentMessageRole, text: &str| AgentMessage {
        message_id: format!("sum-{seq}"),
        run_id: Some("sum-run".into()),
        conversation_id: Some(conversation_id.clone()),
        seq: Some(seq),
        kind: Some(MessageKind::Chat),
        role,
        blocks: vec![AgentMessageBlock::Text { text: text.into() }],
        created_at: Utc::now(),
    };
    // Persist the conversation as seed history.
    let seed = vec![
        mk(0, AgentMessageRole::User, "我关注两只标的：贵州茅台(600519.SH)和招商银行(600036.SH)。"),
        mk(1, AgentMessageRole::Assistant, "好的，已记下贵州茅台与招商银行两只关注标的。"),
        mk(2, AgentMessageRole::User, "我已经建立的判断是：茅台估值偏高暂时观望，招行可以逢低分批买入。"),
        mk(3, AgentMessageRole::Assistant, "明白。茅台观望、招行逢低分批，是你已建立的判断。"),
        mk(4, AgentMessageRole::User, "我的风险纪律是单只个股仓位不超过总仓位的20%，且不做融资融券。"),
        mk(5, AgentMessageRole::Assistant, "收到，单票上限20%、不碰两融，是你的风险纪律。"),
        mk(6, AgentMessageRole::User, "还有一个未决问题：招行的分批买入点位我还没想清楚，需要再观察。"),
    ];
    for m in &seed {
        repo.upsert_message(m).unwrap();
    }

    // Trigger a real follow-up turn that forces Summarize via tiny thresholds + summarize_prompt.
    // New contract: the prior conversation above is the persisted fixture; hand only the new
    // user message — the engine persists it then loads the full prior history as turn context.
    let summarize_prompt = "你是会话压缩器。请用中文把以下对话压缩成一段要点摘要，必须覆盖以下要点：\
        关注标的、已建立的判断、未决问题、风险纪律、用户偏好。只输出摘要正文，不要添加额外解释。";

    let provider = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req = AgentRunRequest {
        run_id: "sum-run2".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 1,
        input: vec![user_message("sum-run2", "请基于以上信息继续。")],
        conversation_id: Some(conversation_id.clone()),
        compaction: Some(CompactionConfig {
            soft_limit_tokens: Some(1),
            summarize_threshold_tokens: Some(1),
            hard_limit_tokens: Some(1_000_000),
            keep_recent_turns: Some(1),
            summarize_prompt: Some(summarize_prompt.into()),
            compact_channel: None,
        }),
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
    };
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let pump = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    // 用带持久化的入口，Summarize 产出的 kind=Summary 检查点才会落到 repo（供下方读回判定）。
    let _ = run_agent_turn(
        req,
        registry,
        ContextBundle::new("sum-run2"),
        vec![provider],
        tx,
        Some(repo.clone()),
    )
    .await
    .expect("summarize run");
    pump.await.unwrap();

    // Read back the produced Summary message text.
    let all = repo.load_conversation(&conversation_id).unwrap();
    let summary_text = all
        .iter()
        .find(|m| m.kind == Some(MessageKind::Summary))
        .and_then(|m| m.blocks.first())
        .map(|b| match b {
            AgentMessageBlock::Text { text } => text.clone(),
            _ => String::new(),
        })
        .unwrap_or_default();
    println!("[judge_summary_faithfulness][{}] summary={:?}", ch.label, summary_text);
    assert!(!summary_text.trim().is_empty(), "no Summary checkpoint produced");

    let scenario = format!(
        "An agent compressed a multi-turn investment conversation into a summary checkpoint. \
        The ORIGINAL conversation was:\n{original_facts}\n\nThe summary must faithfully capture \
        the key facts with no hallucinations."
    );
    let rubric = "1) 摘要忠实反映原对话的关键事实，没有编造原文没有的事实；\
        2) 覆盖关注标的（茅台600519.SH 与 招行600036.SH）；\
        3) 覆盖已建立的判断（茅台观望、招行逢低分批）；\
        4) 覆盖未决问题（招行分批买入点位未定）；\
        5) 覆盖风险纪律（单票≤20%、不做两融）。";
    let v = judge(&judge_ch, &scenario, &summary_text, rubric)
        .await
        .unwrap_or_else(|e| panic!("judge error: {e}"));
    assert_verdict("summary_faithfulness", ch.label, &v);
}

// ===========================================================================
// 5. memory_through_compaction — after a Summarize compaction, ask about a fact stated BEFORE
//    the summary boundary; judge that the answer still correctly recalls the earlier fact
//    (preserved via the summary).
// ===========================================================================
#[tokio::test]
#[ignore]
async fn judge_memory_through_compaction() {
    let Some(judge_ch) = judge_channel() else {
        println!("[judge_memory_through_compaction] skip: set JUDGE_*");
        return;
    };
    let Some(ch) = pick_fast_agent_channel() else {
        println!("[judge_memory_through_compaction] skip: set an agent channel");
        return;
    };

    let repo = fresh_repo();
    let registry = Arc::new(ToolRegistry::new_without_persist());
    let conversation_id = "judge-compact-conv".to_string();

    // Earlier fact stated BEFORE the summary boundary: account number.
    let mk = |seq: i64, role: AgentMessageRole, text: &str| AgentMessage {
        message_id: format!("cmp-{seq}"),
        run_id: Some("cmp-1".into()),
        conversation_id: Some(conversation_id.clone()),
        seq: Some(seq),
        kind: Some(MessageKind::Chat),
        role,
        blocks: vec![AgentMessageBlock::Text { text: text.into() }],
        created_at: Utc::now(),
    };
    let history = vec![
        mk(0, AgentMessageRole::User, "记住一个关键事实：我的模拟账户代号是 ACC-7788。后面我会问你。"),
        mk(1, AgentMessageRole::Assistant, "好的，已记住你的模拟账户代号是 ACC-7788。"),
        mk(2, AgentMessageRole::User, "另外我关注比亚迪(002594.SZ)，主要看新能源车销量。"),
        mk(3, AgentMessageRole::Assistant, "明白，比亚迪(002594.SZ)，关注新能源车销量。"),
    ];
    for m in &history {
        repo.upsert_message(m).unwrap();
    }

    // Turn that forces Summarize AND asks about the earlier fact in one shot. keep_recent=1 keeps
    // only the trailing question in the tail window; the account-number turn is summarized.
    // New contract: hand only the new question; the engine persists it then loads the
    // (now-summarized) prior history fixture as turn context.
    let q = user_message("cmp-2", "我前面告诉过你的模拟账户代号是多少？请直接回答那个代号。");

    let summarize_prompt = "你是会话压缩器。请用中文把以下对话压缩成要点摘要，务必完整保留对话中提到的\
        所有关键事实（包括账户代号、关注标的等具体值）。只输出摘要正文。";

    let provider = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req = AgentRunRequest {
        run_id: "cmp-2".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 1,
        input: vec![q],
        conversation_id: Some(conversation_id.clone()),
        compaction: Some(CompactionConfig {
            soft_limit_tokens: Some(1),
            summarize_threshold_tokens: Some(1),
            hard_limit_tokens: Some(1_000_000),
            keep_recent_turns: Some(1),
            summarize_prompt: Some(summarize_prompt.into()),
            compact_channel: None,
        }),
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
    };
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let pump = tokio::spawn(async move {
        let mut text = String::new();
        let mut saw_summarize = false;
        while let Some(e) = rx.recv().await {
            match e {
                AgentEvent::TextDelta { delta, .. } => text.push_str(&delta),
                AgentEvent::Compacted { tier, .. } => {
                    if matches!(tier, crate::domain::agent::CompactedTier::Summarize) {
                        saw_summarize = true;
                    }
                }
                _ => {}
            }
        }
        (text, saw_summarize)
    });
    let _ = run_agent_turn(req, registry, ContextBundle::new("cmp-2"), vec![provider], tx, Some(repo.clone()))
        .await
        .expect("compaction run");
    let (answer, saw_summarize) = pump.await.unwrap();
    println!(
        "[judge_memory_through_compaction][{}] saw_summarize={saw_summarize} answer={:?}",
        ch.label, answer
    );
    assert!(saw_summarize, "expected a Summarize compaction to have fired");
    assert!(!answer.is_empty(), "empty answer");

    let scenario = "Earlier in the conversation (now folded into a summary checkpoint), the user \
        said their simulated account code is ACC-7788. After compaction the user asked the agent \
        to recall that account code. The answer must still know it (preserved via the summary).";
    let rubric = "1) 回答正确给出账户代号 ACC-7788；\
        2) 没有声称不知道/忘记了；\
        3) 没有编造一个不同的代号。";
    let v = judge(&judge_ch, scenario, &answer, rubric)
        .await
        .unwrap_or_else(|e| panic!("judge error: {e}"));
    assert_verdict("memory_through_compaction", ch.label, &v);
}

// ===========================================================================
// 6. durable_fact_preserved — register place_order (SideEffect::TradingWrite) returning
//    {orderId, fillPrice}; have the model call it; force compaction; ask for the order id;
//    judge the answer still knows it. Also deterministically assert the trading_write
//    tool_result survived inline in the conversation messages (not stubbed/dropped).
// ===========================================================================
#[tokio::test]
#[ignore]
async fn judge_durable_fact_preserved() {
    let Some(judge_ch) = judge_channel() else {
        println!("[judge_durable_fact_preserved] skip: set JUDGE_*");
        return;
    };
    let Some(ch) = pick_fast_agent_channel() else {
        println!("[judge_durable_fact_preserved] skip: set an agent channel");
        return;
    };

    let repo = fresh_repo();
    // ToolRegistry needs persistence for ToolCall audit; reuse the repo's db for a PayloadStore.
    let registry = Arc::new(ToolRegistry::new_without_persist());
    let order_id = "ORD-55123";
    let fill_price = "1801.0";
    let spec = ToolSpec::new(
        "place_order",
        "在模拟账户下单买入某标的。下单后会返回订单号(orderId)和成交价(fillPrice)。当用户要求买入某标的时调用本 tool。",
        json!({"type":"object","properties":{"tsCode":{"type":"string"},"side":{"type":"string"},"qty":{"type":"integer"}},"required":["tsCode"]}),
        vec![r#"<use_tool name="place_order">{"tsCode":"600519.SH","side":"buy","qty":100}</use_tool>"#.to_string()],
        5000,
        SideEffect::TradingWrite,
    );
    let oid = order_id.to_string();
    let fp = fill_price.to_string();
    let handler: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(move |_inv: ToolInvocation| {
        let oid = oid.clone();
        let fp = fp.clone();
        Box::pin(async move {
            ToolHandlerOutput::ok(json!({"orderId": oid, "fillPrice": fp, "status": "filled"}))
        }) as ToolHandlerFuture
    }));
    registry.register_tool(spec, handler).unwrap();

    let conversation_id = "judge-durable-conv".to_string();

    // Turn 1: ask the model to place an order (it must call place_order, getting the durable
    // orderId). New contract: hand only the new user message — the engine persists it + the
    // produced turn under the conversation.
    let provider1 = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req1 = AgentRunRequest {
        run_id: "dur-1".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 4,
        input: vec![user_message(
            "dur-1",
            "请用 place_order 帮我以市价买入 100 股 600519.SH，下单后告诉我订单号。",
        )],
        conversation_id: Some(conversation_id.clone()),
        compaction: None,
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
    };
    let (tx1, mut rx1) = mpsc::channel::<AgentEvent>(256);
    let pump1 = tokio::spawn(async move {
        let mut tools: Vec<(String, bool)> = Vec::new();
        while let Some(e) = rx1.recv().await {
            if let AgentEvent::ToolEnd { name, is_error, .. } = e {
                tools.push((name, is_error));
            }
        }
        tools
    });
    let _ = run_agent_turn(
        req1,
        registry.clone(),
        ContextBundle::new("dur-1"),
        vec![provider1],
        tx1,
        Some(repo.clone()),
    )
    .await
    .expect("turn 1 (place_order)");
    let t1_tools = pump1.await.unwrap();
    println!("[judge_durable_fact_preserved][{}] turn1 tools={:?}", ch.label, t1_tools);
    assert!(
        t1_tools.iter().any(|(n, err)| n == "place_order" && !err),
        "[{}] place_order was not dispatched successfully",
        ch.label
    );

    // Turn 2: NEW run, same conversation, FORCE compaction, ask for the order id. New contract:
    // hand only the follow-up question — the engine persists it, loads the prior turn (with the
    // durable place_order result) and applies tiny thresholds + summarize_prompt to compact.
    let summarize_prompt = "你是会话压缩器。请用中文把以下对话压缩成要点摘要。只输出摘要正文。";
    let provider2 = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req2 = AgentRunRequest {
        run_id: "dur-2".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 2,
        input: vec![user_message("dur-2", "我刚才下的那笔订单的订单号是多少？请直接回答订单号。")],
        conversation_id: Some(conversation_id.clone()),
        compaction: Some(CompactionConfig {
            soft_limit_tokens: Some(1),
            summarize_threshold_tokens: Some(1),
            hard_limit_tokens: Some(1_000_000),
            keep_recent_turns: Some(1),
            summarize_prompt: Some(summarize_prompt.into()),
            compact_channel: None,
        }),
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
    };
    let (tx2, mut rx2) = mpsc::channel::<AgentEvent>(256);
    let pump2 = tokio::spawn(async move {
        let mut text = String::new();
        let mut saw_compaction = false;
        while let Some(e) = rx2.recv().await {
            match e {
                AgentEvent::TextDelta { delta, .. } => text.push_str(&delta),
                AgentEvent::Compacted { .. } => saw_compaction = true,
                _ => {}
            }
        }
        (text, saw_compaction)
    });
    let _ = run_agent_turn(
        req2,
        registry,
        ContextBundle::new("dur-2"),
        vec![provider2],
        tx2,
        Some(repo.clone()),
    )
    .await
    .expect("turn 2 (recall after compaction)");
    let (answer, saw_compaction) = pump2.await.unwrap();
    println!(
        "[judge_durable_fact_preserved][{}] saw_compaction={saw_compaction} answer={:?}",
        ch.label, answer
    );
    assert!(!answer.is_empty(), "empty answer");

    // Deterministic guard (spec §2/§4): the trading_write tool_result must survive inline in the
    // persisted conversation — it is a durable fact that compaction must never stub/drop. The full
    // <tool_result> text carrying the orderId must still be present in agent_messages.
    let all = repo.load_conversation(&conversation_id).unwrap();
    let order_id_inline = all.iter().any(|m| {
        m.blocks.iter().any(|b| match b {
            AgentMessageBlock::Text { text } => {
                text.contains("tool_result") && text.contains(order_id)
            }
            _ => false,
        })
    });
    assert!(
        order_id_inline,
        "trading_write tool_result with orderId {order_id} must survive inline (not stubbed/dropped) in persisted conversation"
    );

    let scenario = format!(
        "The agent placed a simulated order earlier; the trading tool (place_order, a trading-write \
        side effect) returned orderId={order_id}. The conversation was then compacted. The user \
        asked for the order id. The answer must still know it (trading-write results are never \
        dropped by compaction)."
    );
    let rubric = format!(
        "1) 回答正确给出订单号 {order_id}；\
        2) 没有声称忘记/不知道订单号；\
        3) 没有编造一个不同的订单号。"
    );
    let v = judge(&judge_ch, &scenario, &answer, &rubric)
        .await
        .unwrap_or_else(|e| panic!("judge error: {e}"));
    assert_verdict("durable_fact_preserved", ch.label, &v);
}

// 7. rolling_summary — a SECOND summarize must FOLD the prior summary checkpoint
//    (long-term: multi-cycle compaction must not lose the earliest history).
#[tokio::test]
#[ignore]
async fn judge_rolling_summary_folds_prior() {
    let Some(judge_ch) = judge_channel() else {
        println!("[judge_rolling_summary] skip: set JUDGE_*");
        return;
    };
    let Some(ch) = pick_fast_agent_channel() else {
        println!("[judge_rolling_summary] skip: set an agent channel");
        return;
    };

    let repo = fresh_repo();
    let registry = Arc::new(ToolRegistry::new_without_persist());
    let conversation_id = "judge-rolling-conv".to_string();
    let mk = |seq: i64, role: AgentMessageRole, kind: Option<MessageKind>, text: &str| AgentMessage {
        message_id: format!("roll-{seq}"),
        run_id: Some("roll".into()),
        conversation_id: Some(conversation_id.clone()),
        seq: Some(seq),
        kind,
        role,
        blocks: vec![AgentMessageBlock::Text { text: text.into() }],
        created_at: Utc::now(),
    };

    // Pre-seed a PRIOR summary checkpoint carrying a distinctive early fact (ACC-9001),
    // then new turns after it. The next summarize must fold the prior summary in.
    let prior_summary = "前情摘要：用户账户代码 ACC-9001；关注标的 贵州茅台(600519.SH)；风险纪律 单票仓位≤20%、不做两融。";
    let seed = vec![
        mk(0, AgentMessageRole::Assistant, Some(MessageKind::Summary), prior_summary),
        mk(1, AgentMessageRole::User, Some(MessageKind::Chat), "我新增关注招商银行(600036.SH)，想逢低分批买。"),
        mk(2, AgentMessageRole::Assistant, Some(MessageKind::Chat), "好的，已记下新增关注招商银行、逢低分批。"),
        mk(3, AgentMessageRole::User, Some(MessageKind::Chat), "还有个未决问题：招行的买入点位我还没定。"),
    ];
    for m in &seed {
        repo.upsert_message(m).unwrap();
    }

    // New contract: the prior summary + chats above are the persisted fixture (an
    // already-compressed conversation state). Hand only the new user message — the engine
    // persists it, loads the compressed view (prior summary + tail + input) and re-summarizes.
    let summarize_prompt = "你是会话压缩器。请产出一份完整的中文累积摘要：若输入里已有'前情摘要'，必须把它的内容与后续新对话合并，不得遗漏旧信息。必须覆盖：账户/标的、已建立判断、未决问题、风险纪律。只输出摘要正文。";

    let provider = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req = AgentRunRequest {
        run_id: "roll".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 1,
        input: vec![user_message("roll", "请基于以上继续。")],
        conversation_id: Some(conversation_id.clone()),
        compaction: Some(CompactionConfig {
            soft_limit_tokens: Some(1),
            summarize_threshold_tokens: Some(1),
            hard_limit_tokens: Some(1_000_000),
            keep_recent_turns: Some(1),
            summarize_prompt: Some(summarize_prompt.into()),
            // Use a capable compaction model (realistic — Runtime configures a decent
            // compact_channel; a weak summarizer may drop folded facts).
            compact_channel: Some(judge_ch.clone()),
        }),
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
    };
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let pump = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let _ = run_agent_turn(
        req,
        registry,
        ContextBundle::new("roll"),
        vec![provider],
        tx,
        Some(repo.clone()),
    )
    .await
    .expect("rolling summarize run");
    pump.await.unwrap();

    // Deterministic: exactly ONE summary remains (rolling replaced the prior) and it still
    // carries the earliest fact (ACC-9001) — proves the prior summary was folded, not lost.
    let view = repo.load_conversation_view(&conversation_id).unwrap();
    let summaries: Vec<&AgentMessage> =
        view.iter().filter(|m| m.kind == Some(MessageKind::Summary)).collect();
    assert_eq!(summaries.len(), 1, "rolling summary must keep exactly one checkpoint");
    let summary_text = summaries[0]
        .blocks
        .first()
        .map(|b| match b {
            AgentMessageBlock::Text { text } => text.clone(),
            _ => String::new(),
        })
        .unwrap_or_default();
    println!("[judge_rolling_summary][{}] rolling summary={:?}", ch.label, summary_text);
    assert!(
        summary_text.contains("ACC-9001"),
        "prior-summary fact ACC-9001 lost — rolling fold failed: {summary_text:?}"
    );

    let scenario = format!(
        "A prior summary checkpoint was:\n{prior_summary}\n\nThen new turns added: \
        新增关注招商银行(600036.SH) 逢低分批；未决问题 = 招行买入点位未定。\n\nThe agent produced a \
        NEW rolling summary that must MERGE the prior summary AND the new turns."
    );
    let rubric = "1) 保留前情摘要的事实：账户代码 ACC-9001、关注贵州茅台(600519.SH)、风险纪律单票≤20%且不做两融；\
        2) 纳入新事实：新增关注招商银行(600036.SH)、逢低分批；\
        3) 纳入未决问题：招行买入点位未定；4) 无编造。";
    let v = judge(&judge_ch, &scenario, &summary_text, rubric)
        .await
        .unwrap_or_else(|e| panic!("judge error: {e}"));
    assert_verdict("rolling_summary", ch.label, &v);
}

// ===========================================================================
// Shared helpers for the extended A/B suite (multi-cycle compaction, constraint
// accumulation, durable-vs-droppable, keep_recent verbatim, Drop-degrade).
// Private to this file; keep the same style as the helpers above.
// ===========================================================================

/// A "tight" CompactionConfig: token thresholds = 1 so even a one-line conversation triggers
/// compaction every turn; `keep_recent` + summarize prompt + (optional) compact channel are the
/// knobs each test varies. This is how the suite forces real compaction cheaply (no 万-token text).
fn tight_compaction(
    keep_recent: u32,
    summarize_prompt: Option<&str>,
    compact_channel: Option<ProviderChannel>,
) -> CompactionConfig {
    CompactionConfig {
        soft_limit_tokens: Some(1),
        summarize_threshold_tokens: Some(1),
        hard_limit_tokens: Some(1_000_000),
        keep_recent_turns: Some(keep_recent),
        summarize_prompt: summarize_prompt.map(|s| s.to_string()),
        compact_channel,
    }
}

/// Build a `kind=Chat` conversation message with an explicit seq under a conversation.
fn conv_chat(
    conversation_id: &str,
    seq: i64,
    role: AgentMessageRole,
    text: &str,
) -> AgentMessage {
    AgentMessage {
        message_id: format!("{conversation_id}-{seq}"),
        run_id: Some(conversation_id.to_string()),
        conversation_id: Some(conversation_id.to_string()),
        seq: Some(seq),
        kind: Some(MessageKind::Chat),
        role,
        blocks: vec![AgentMessageBlock::Text { text: text.into() }],
        created_at: Utc::now(),
    }
}

/// Pull the (single) `kind=Summary` checkpoint text out of a conversation's full audit log.
/// Returns "" when there is none.
fn summary_text_of(repo: &AgentMessagesRepo, conversation_id: &str) -> String {
    repo.load_conversation(conversation_id)
        .unwrap()
        .iter()
        .rev()
        .find(|m| m.kind == Some(MessageKind::Summary))
        .and_then(|m| m.blocks.first())
        .map(|b| match b {
            AgentMessageBlock::Text { text } => text.clone(),
            _ => String::new(),
        })
        .unwrap_or_default()
}

/// Count how many summary checkpoints exist in the FULL audit log (each Summarize cycle that
/// folds the prior one keeps exactly one *live* summary in the view, but the audit log will hold
/// every checkpoint ever written; we count audit-log summaries to prove ≥N cycles fired).
fn audit_summary_count(repo: &AgentMessagesRepo, conversation_id: &str) -> usize {
    repo.load_conversation(conversation_id)
        .unwrap()
        .iter()
        .filter(|m| m.kind == Some(MessageKind::Summary))
        .count()
}

/// Run one turn that forces a Summarize over the given conversation. New contract: hands the
/// engine only this round's new user nudge (the engine persists it + loads the compressed view as
/// the turn context itself), with a tight compaction + summarize prompt; drains events. Returns
/// whether a Summarize tier fired.
async fn force_one_summarize_cycle(
    repo: &AgentMessagesRepo,
    channel: &ProviderChannel,
    conversation_id: &str,
    run_id: &str,
    nudge_text: &str,
    summarize_prompt: &str,
    compact_channel: Option<ProviderChannel>,
) -> bool {
    let provider = Box::new(HttpProvider::new(channel.clone()).unwrap());
    let req = AgentRunRequest {
        run_id: run_id.into(),
        trigger: "user".into(),
        channel: channel.clone(),
        max_turns: 1,
        input: vec![user_message(run_id, nudge_text)],
        conversation_id: Some(conversation_id.to_string()),
        compaction: Some(tight_compaction(1, Some(summarize_prompt), compact_channel)),
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
    };
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let pump = tokio::spawn(async move {
        let mut saw = false;
        while let Some(e) = rx.recv().await {
            if let AgentEvent::Compacted { tier, .. } = e {
                if matches!(tier, crate::domain::agent::CompactedTier::Summarize) {
                    saw = true;
                }
            }
        }
        saw
    });
    let _ = run_agent_turn(
        req,
        Arc::new(ToolRegistry::new_without_persist()),
        ContextBundle::new(run_id),
        vec![provider],
        tx,
        Some(repo.clone()),
    )
    .await
    .expect("summarize cycle");
    pump.await.unwrap()
}

/// Run ONE conversational turn with transient-retry resilience (for long stress runs over a flaky
/// relay). Hands the engine only this round's new user message; the engine persists it + loads the
/// compressed view + persists outputs. Drains events, counting Summarize cycles and collecting the
/// streamed answer. On a transient 5xx / upstream / timeout it retries the turn (small backoff);
/// non-transient errors return ok=false. Returns (ok, summarize_cycles_fired, answer_text).
async fn run_turn_resilient(
    repo: &AgentMessagesRepo,
    channel: &ProviderChannel,
    conversation_id: &str,
    run_id: &str,
    text: &str,
    compaction: &CompactionConfig,
) -> (bool, u32, String) {
    for attempt in 0..6u32 {
        let provider = Box::new(HttpProvider::new(channel.clone()).unwrap());
        let req = AgentRunRequest {
            run_id: format!("{run_id}-a{attempt}"),
            trigger: "user".into(),
            channel: channel.clone(),
            max_turns: 1,
            input: vec![user_message(run_id, text)],
            conversation_id: Some(conversation_id.to_string()),
            compaction: Some(compaction.clone()),
            fallback_channels: vec![],
            retry: None,
            token_budget: None,
        };
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
        let pump = tokio::spawn(async move {
            let mut fired = 0u32;
            let mut answer = String::new();
            while let Some(e) = rx.recv().await {
                match e {
                    AgentEvent::Compacted {
                        tier: crate::domain::agent::CompactedTier::Summarize,
                        ..
                    } => fired += 1,
                    AgentEvent::TextDelta { delta, .. } => answer.push_str(&delta),
                    _ => {}
                }
            }
            (fired, answer)
        });
        let res = run_agent_turn(
            req,
            Arc::new(ToolRegistry::new_without_persist()),
            ContextBundle::new(run_id),
            vec![provider],
            tx,
            Some(repo.clone()),
        )
        .await;
        let (fired, answer) = pump.await.unwrap();
        match res {
            Ok(_) => return (true, fired, answer),
            Err(e) => {
                let msg = format!("{e}").to_lowercase();
                let transient = msg.contains("provider returned 5") // any 5xx
                    || msg.contains("upstream")
                    || msg.contains("timed out")
                    || msg.contains("timeout")
                    || msg.contains("connection");
                if transient && attempt < 5 {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    continue;
                }
                eprintln!("[100turn] turn {run_id} failed (non-retryable or retries exhausted): {e}");
                return (false, fired, answer);
            }
        }
    }
    (false, 0, String::new())
}

// ---------------------------------------------------------------------------
// A0. judge_hundred_turn_longterm_memory — 100+ 轮压测：第 1 轮埋账户代号、第 50 轮埋纪律口令，
//     中间 tight_compaction 让 Summarize **每轮**触发（≈100 次滚动折叠 = 摘要"传话游戏"压测），
//     最后让 agent 同时召回两个早期事实。验证：①持久化在规模下不丢（审计全量增长）；②上下文
//     恒定收敛（view 不随轮数增长）；③早期事实穿越上百次滚动折叠仍逐字在；④judge 召回准确。
//     summarizer 用 judge(opus) 渠道（可靠），agent 用被测渠道；单轮带瞬时重试抗 relay 抖动。
// ---------------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn judge_hundred_turn_longterm_memory() {
    let Some(judge_ch) = judge_channel() else {
        eprintln!("[judge_hundred_turn_longterm_memory] skipped: channels not configured (JUDGE_*)");
        return;
    };
    let Some(ch) = pick_fast_agent_channel() else {
        eprintln!("[judge_hundred_turn_longterm_memory] skipped: channels not configured (agent)");
        return;
    };

    let repo = fresh_repo();
    let conv = "judge-100turn-conv".to_string();
    const ACCOUNT: &str = "ACC-7711";
    const PASSPHRASE: &str = "BLUE-OWL-42";

    let summarize_prompt = "你是会话压缩器。产出一份完整的中文累积摘要：若输入里含『已有摘要』，\
        必须把其中的全部具体值（尤其是账户代号 ACC-xxxx、纪律口令等关键标识）逐字保留，并与后续\
        新对话合并，绝不遗漏或改写早期事实。无关的流水笔记可概括。只输出摘要正文。";
    // tight = Summarize fires basically every turn → maximal rolling-fold stress on the early facts.
    let compaction = tight_compaction(4, Some(summarize_prompt), Some(judge_ch.clone()));

    const TOTAL: u32 = 105;
    let mut ok_turns = 0u32;
    let mut failed_turns = 0u32;
    let mut summarize_cycles = 0u32;

    for i in 1..=TOTAL {
        let text = if i == 1 {
            format!("请牢记第 1 条关键信息：我的模拟账户代号是 {ACCOUNT}。后面我会持续追加很多条笔记，但这条要一直记住。")
        } else if i == 50 {
            format!("第 50 条，追加一条同样关键的纪律口令：{PASSPHRASE}，请和账户代号一样长期记牢。")
        } else {
            format!("第 {i} 条笔记：随手记录一条市场观察 #{i}，不重要，简短确认收到即可。")
        };
        let (ok, fired, _) =
            run_turn_resilient(&repo, &ch.channel, &conv, &format!("ht-{i}"), &text, &compaction).await;
        if ok {
            ok_turns += 1;
        } else {
            failed_turns += 1;
        }
        summarize_cycles += fired;
        if i == 1 || i == 50 || i % 20 == 0 {
            let view = repo.load_conversation_view(&conv).unwrap();
            eprintln!(
                "[100turn][{}] turn={i} ok={ok_turns} fail={failed_turns} cycles={summarize_cycles} view_len={} summary={:?}",
                ch.label,
                view.len(),
                summary_text_of(&repo, &conv)
            );
        }
    }

    let full = repo.load_conversation(&conv).unwrap();
    let view = repo.load_conversation_view(&conv).unwrap();
    let live_summary = summary_text_of(&repo, &conv);
    eprintln!(
        "[100turn] DONE ok={ok_turns}/{TOTAL} fail={failed_turns} cycles={summarize_cycles} full_audit={} view_len={}\n  final_summary={:?}",
        full.len(),
        view.len(),
        live_summary
    );

    // ① persistence at scale: the full audit log holds (most of) the turns.
    assert!(
        ok_turns >= 100,
        "expected ≥100 successful turns (got {ok_turns}; {failed_turns} exhausted retries — relay too flaky to conclude)"
    );
    assert!(
        full.len() >= 150,
        "persistence at scale: full audit should accumulate all turns (got {})",
        full.len()
    );
    // ② many rolling Summarize cycles actually happened.
    assert!(
        summarize_cycles >= 30,
        "expected many rolling Summarize cycles across 100 turns (got {summarize_cycles})"
    );
    // ③ context stayed bounded — the view does NOT grow with turn count.
    assert!(
        view.len() <= 30,
        "context must stay bounded via compaction, not grow with turns (view={}, full_audit={})",
        view.len(),
        full.len()
    );
    // ④ both early facts survived ~100 rolling folds, verbatim, in the live summary.
    assert!(
        live_summary.contains(ACCOUNT),
        "turn-1 fact {ACCOUNT} lost from rolling summary after {TOTAL} turns: {live_summary:?}"
    );
    assert!(
        live_summary.contains(PASSPHRASE),
        "turn-50 fact {PASSPHRASE} lost from rolling summary after {TOTAL} turns: {live_summary:?}"
    );

    // Final recall, judged by the LLM.
    let q = "请直接回答两个问题：1) 我在第 1 条告诉你的模拟账户代号是多少？\
        2) 我在中途（第 50 条）给你的纪律口令是什么？";
    let (ok, _, answer) =
        run_turn_resilient(&repo, &ch.channel, &conv, "ht-final", q, &compaction).await;
    assert!(ok, "final recall turn failed");
    eprintln!("[100turn] final answer={answer:?}");

    let scenario = format!(
        "A 100+ turn conversation. In turn 1 the user stated their simulated account code {ACCOUNT}; \
         at turn 50 they added a discipline passphrase {PASSPHRASE}. Dozens of rolling summarization \
         cycles happened in between (the context was repeatedly compacted). The user now asks the \
         agent to recall BOTH early facts."
    );
    let rubric = format!(
        "1) 回答里准确给出账户代号 {ACCOUNT}；2) 准确给出纪律口令 {PASSPHRASE}；\
         3) 没有编造不同的值，也没有声称忘记/不知道。三条全满足才算 pass。"
    );
    let v = judge(&judge_ch, &scenario, &answer, &rubric)
        .await
        .unwrap_or_else(|e| panic!("judge error: {e}"));
    assert_verdict("hundred_turn_longterm_memory", ch.label, &v);
}

// ===========================================================================
// ===== A. 长期记忆保留 =====
// ===========================================================================

// ---------------------------------------------------------------------------
// A1. judge_longterm_fact_survives_multiple_cycles — 埋一个第 1 轮事实(ACC-3344)，
//     强制 *多次*(≥2) 滚动 Summarize 周期，最后让 agent 召回该事实。
//     Deterministic guard: 审计日志里 summary 检查点 ≥2(证明多周期发生) 且 *当前* view 的
//     滚动摘要仍含 ACC-3344(证明早期事实穿越多次压缩没丢)。再由 judge 验证 agent 答得准。
//     [矩阵 A1]
// ---------------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn judge_longterm_fact_survives_multiple_cycles() {
    let Some(judge_ch) = judge_channel() else {
        eprintln!("[judge_longterm_fact_survives_multiple_cycles] skipped: channels not configured (JUDGE_*)");
        return;
    };
    let Some(ch) = pick_fast_agent_channel() else {
        eprintln!("[judge_longterm_fact_survives_multiple_cycles] skipped: channels not configured (agent)");
        return;
    };

    let repo = fresh_repo();
    let conversation_id = "judge-longterm-conv".to_string();

    // Turn 1 fact (the earliest, must survive every cycle).
    let seed = vec![
        conv_chat(&conversation_id, 0, AgentMessageRole::User, "请牢记一个关键事实：我的模拟账户代号是 ACC-3344。后面我会反复追加别的信息。"),
        conv_chat(&conversation_id, 1, AgentMessageRole::Assistant, "好的，已牢记你的模拟账户代号是 ACC-3344。"),
    ];
    for m in &seed {
        repo.upsert_message(m).unwrap();
    }

    // A capable summarizer is required for faithful rolling folds; use the judge channel as the
    // compact model (realistic — Runtime configures a decent compact_channel).
    let summarize_prompt = "你是会话压缩器。请产出一份完整的中文累积摘要：若输入里已有'已有摘要'，\
        必须把它的全部事实(尤其账户代号等具体值)与后续新对话合并，逐字保留具体值，不得遗漏旧信息。\
        只输出摘要正文。";

    // Drive ≥2 summarize cycles, each adding a new fact + nudging compaction.
    let cycle_inputs = [
        ("lt-c1", "我新增关注贵州茅台(600519.SH)，请继续。"),
        ("lt-c2", "我的风险纪律是单票仓位不超过20%，请继续。"),
        ("lt-c3", "我还排除工商银行(601398.SH)，请继续。"),
    ];
    let mut cycles_fired = 0;
    for (run_id, nudge) in cycle_inputs {
        let fired = force_one_summarize_cycle(
            &repo,
            &ch.channel,
            &conversation_id,
            run_id,
            nudge,
            summarize_prompt,
            Some(judge_ch.clone()),
        )
        .await;
        if fired {
            cycles_fired += 1;
        }
        eprintln!(
            "[judge_longterm_fact_survives_multiple_cycles][{}] cycle={run_id} fired={fired} audit_summaries={} live_summary={:?}",
            ch.label,
            audit_summary_count(&repo, &conversation_id),
            summary_text_of(&repo, &conversation_id),
        );
    }

    assert!(
        cycles_fired >= 2,
        "expected ≥2 Summarize cycles to fire (got {cycles_fired}) — multi-cycle long-term test"
    );
    let live_summary = summary_text_of(&repo, &conversation_id);
    assert!(
        live_summary.contains("ACC-3344"),
        "earliest fact ACC-3344 lost after multi-cycle rolling compaction — long-term history dropped: {live_summary:?}"
    );

    // Final turn: ask the agent to recall the earliest fact, continuing from the (now heavily
    // compacted) view. New contract: hand only the new question — engine persists + loads view.
    let provider = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req = AgentRunRequest {
        run_id: "lt-final".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 1,
        input: vec![user_message("lt-final", "我最开始告诉你的模拟账户代号是多少？请直接回答那个代号。")],
        conversation_id: Some(conversation_id.clone()),
        compaction: Some(tight_compaction(1, Some(summarize_prompt), Some(judge_ch.clone()))),
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
    };
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let pump = tokio::spawn(async move {
        let mut text = String::new();
        while let Some(e) = rx.recv().await {
            if let AgentEvent::TextDelta { delta, .. } = e {
                text.push_str(&delta);
            }
        }
        text
    });
    let _ = run_agent_turn(
        req,
        Arc::new(ToolRegistry::new_without_persist()),
        ContextBundle::new("lt-final"),
        vec![provider],
        tx,
        Some(repo.clone()),
    )
    .await
    .expect("final recall turn");
    let answer = pump.await.unwrap();
    eprintln!("[judge_longterm_fact_survives_multiple_cycles][{}] final answer={:?}", ch.label, answer);
    assert!(!answer.is_empty(), "empty final answer");

    let scenario = "A long conversation underwent multiple rolling-summary compaction cycles. The \
        EARLIEST fact was: the simulated account code is ACC-3344. After several summarize cycles the \
        user asked to recall that account code. The answer must still know it.";
    let rubric = "1) 回答正确给出账户代号 ACC-3344；\
        2) 没有声称忘记/不知道；3) 没有编造一个不同的代号。";
    let v = judge(&judge_ch, scenario, &answer, rubric)
        .await
        .unwrap_or_else(|e| panic!("judge error: {e}"));
    assert_verdict("longterm_fact_survives_multiple_cycles", ch.label, &v);
}

// ---------------------------------------------------------------------------
// A2. judge_accumulated_constraints — 用户分多轮逐步加约束(只买银行股 → 排除工商银行 →
//     单笔预算≤5万)，每轮持久化续接；最后让 agent 给建议 → judge 是否同时满足全部约束。
//     [矩阵 A2]
// ---------------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn judge_accumulated_constraints() {
    let Some(judge_ch) = judge_channel() else {
        eprintln!("[judge_accumulated_constraints] skipped: channels not configured (JUDGE_*)");
        return;
    };
    let Some(ch) = pick_fast_agent_channel() else {
        eprintln!("[judge_accumulated_constraints] skipped: channels not configured (agent)");
        return;
    };

    let repo = fresh_repo();
    let conversation_id = "judge-constraints-conv".to_string();

    // Each constraint arrives in its own turn, continued via persistence + view reload.
    let turns = [
        ("con-1", "约束一：我只买银行股，别的行业一律不考虑。请记住。"),
        ("con-2", "约束二：在银行股里排除工商银行(601398.SH)，不要推荐它。请记住。"),
        ("con-3", "约束三：我单笔买入预算不超过5万元。请记住。"),
        ("con-final", "现在请基于我前面给的所有约束，推荐一只值得关注的标的，并说明大致买入金额。"),
    ];
    let mut final_answer = String::new();
    for (run_id, text) in turns {
        // New contract: hand only this turn's new user message; the engine persists it and loads
        // the accumulated history (all prior constraints) as the turn context.
        let provider = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
        let req = AgentRunRequest {
            run_id: run_id.into(),
            trigger: "user".into(),
            channel: ch.channel.clone(),
            max_turns: 1,
            input: vec![user_message(run_id, text)],
            conversation_id: Some(conversation_id.clone()),
            compaction: None,
            fallback_channels: vec![],
            retry: None,
            token_budget: None,
        };
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
        let pump = tokio::spawn(async move {
            let mut text = String::new();
            while let Some(e) = rx.recv().await {
                if let AgentEvent::TextDelta { delta, .. } = e {
                    text.push_str(&delta);
                }
            }
            text
        });
        let _ = run_agent_turn(
            req,
            Arc::new(ToolRegistry::new_without_persist()),
            ContextBundle::new(run_id),
            vec![provider],
            tx,
            Some(repo.clone()),
        )
        .await
        .expect("constraint turn");
        final_answer = pump.await.unwrap();
    }
    eprintln!("[judge_accumulated_constraints][{}] final answer={:?}", ch.label, final_answer);
    assert!(!final_answer.is_empty(), "empty final answer");

    let scenario = "Across multiple turns the user added three constraints: (1) only buy bank \
        stocks; (2) within banks, EXCLUDE 工商银行(601398.SH); (3) per-trade budget ≤ 50,000 RMB. \
        The final answer recommends a stock + buy amount. It must respect ALL THREE constraints at once.";
    let rubric = "1) 推荐的是银行股(银行板块)，没有推荐非银行行业；\
        2) 推荐的不是被排除的工商银行(601398.SH)；\
        3) 给出的买入金额不超过5万元(≤50000)；\
        4) 体现出同时记住并满足了全部三条约束。";
    let v = judge(&judge_ch, scenario, &final_answer, rubric)
        .await
        .unwrap_or_else(|e| panic!("judge error: {e}"));
    assert_verdict("accumulated_constraints", ch.label, &v);
}

// ---------------------------------------------------------------------------
// A3. judge_durable_verbatim_vs_droppable — 对比验证：trading_write 的 place_order 结果
//     (orderId/fillPrice) 经过压缩后逐字 inline 保留(且 judge 复述精确)，而同一会话里的
//     droppable get_quote 行情快照被 MicroClear 折成 stub(允许被概述/重新拉取)。
//     Deterministic guards:
//       - 持久化会话里仍含完整 <tool_result ... orderId ... fillPrice>(durable 逐字)
//       - get_quote 的原始 price 数值不再以 <tool_result> inline 形式存在(被 stub 化)
//     [矩阵 A3 + B1 的差异面]
// ---------------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn judge_durable_verbatim_vs_droppable() {
    let Some(judge_ch) = judge_channel() else {
        eprintln!("[judge_durable_verbatim_vs_droppable] skipped: channels not configured (JUDGE_*)");
        return;
    };
    let Some(ch) = pick_fast_agent_channel() else {
        eprintln!("[judge_durable_verbatim_vs_droppable] skipped: channels not configured (agent)");
        return;
    };

    let repo = fresh_repo();
    let registry = Arc::new(ToolRegistry::new_without_persist());
    let conversation_id = "judge-durdrop-conv".to_string();

    // Durable trading_write tool: place_order → orderId/fillPrice.
    let order_id = "ORD-90021";
    let fill_price = "1777.0";
    let place_spec = ToolSpec::new(
        "place_order",
        "在模拟账户下单买入某标的。下单后返回订单号(orderId)和成交价(fillPrice)。当用户要求买入时调用。",
        json!({"type":"object","properties":{"tsCode":{"type":"string"},"side":{"type":"string"},"qty":{"type":"integer"}},"required":["tsCode"]}),
        vec![r#"<use_tool name="place_order">{"tsCode":"600519.SH","side":"buy","qty":100}</use_tool>"#.to_string()],
        5000,
        SideEffect::TradingWrite,
    );
    let (oid, fp) = (order_id.to_string(), fill_price.to_string());
    let place_handler: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(move |_inv: ToolInvocation| {
        let (oid, fp) = (oid.clone(), fp.clone());
        Box::pin(async move {
            ToolHandlerOutput::ok(json!({"orderId": oid, "fillPrice": fp, "status": "filled"}))
        }) as ToolHandlerFuture
    }));
    registry.register_tool(place_spec, place_handler).unwrap();

    // Droppable (SideEffect::None) get_quote → a snapshot price that may be compacted away.
    let quote_price = "1755.5";
    let quote_spec = ToolSpec::new(
        "get_quote",
        "获取某标的实时行情快照(droppable，可被压缩后重新拉取)。需要现价时调用。",
        json!({"type":"object","properties":{"tsCode":{"type":"string"}},"required":["tsCode"]}),
        vec![r#"<use_tool name="get_quote">{"tsCode":"600519.SH"}</use_tool>"#.to_string()],
        5000,
        SideEffect::None,
    );
    let qp = quote_price.to_string();
    let quote_handler: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(move |_inv: ToolInvocation| {
        let qp = qp.clone();
        Box::pin(async move { ToolHandlerOutput::ok(json!({"price": qp})) }) as ToolHandlerFuture
    }));
    registry.register_tool(quote_spec, quote_handler).unwrap();

    // Turn 1: fetch a quote (droppable) AND place an order (durable). Both tool_results land inline.
    // New contract: hand only the new user message; the engine persists it + the produced turn.
    let provider1 = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req1 = AgentRunRequest {
        run_id: "dd-1".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 5,
        input: vec![user_message(
            "dd-1",
            "先用 get_quote 查 600519.SH 现价，再用 place_order 以市价买入 100 股 600519.SH，最后告诉我订单号。",
        )],
        conversation_id: Some(conversation_id.clone()),
        compaction: None,
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
    };
    let (tx1, mut rx1) = mpsc::channel::<AgentEvent>(256);
    let pump1 = tokio::spawn(async move {
        let mut tools: Vec<(String, bool)> = Vec::new();
        while let Some(e) = rx1.recv().await {
            if let AgentEvent::ToolEnd { name, is_error, .. } = e {
                tools.push((name, is_error));
            }
        }
        tools
    });
    let _ = run_agent_turn(
        req1,
        registry.clone(),
        ContextBundle::new("dd-1"),
        vec![provider1],
        tx1,
        Some(repo.clone()),
    )
    .await
    .expect("turn 1 (quote + order)");
    let t1_tools = pump1.await.unwrap();
    eprintln!("[judge_durable_verbatim_vs_droppable][{}] turn1 tools={:?}", ch.label, t1_tools);
    assert!(
        t1_tools.iter().any(|(n, e)| n == "place_order" && !e),
        "[{}] place_order not dispatched", ch.label
    );

    // Turn 2: force compaction (MicroClear stubs the droppable quote; durable order kept inline),
    // then ask for the order id. New contract: hand only the follow-up; engine persists + loads.
    let summarize_prompt = "你是会话压缩器。请用中文把以下对话压缩成要点摘要。只输出摘要正文。";
    let provider2 = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req2 = AgentRunRequest {
        run_id: "dd-2".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 2,
        input: vec![user_message("dd-2", "我刚才那笔订单的订单号是多少？请直接回答订单号。")],
        conversation_id: Some(conversation_id.clone()),
        compaction: Some(tight_compaction(1, Some(summarize_prompt), None)),
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
    };
    let (tx2, mut rx2) = mpsc::channel::<AgentEvent>(256);
    let pump2 = tokio::spawn(async move {
        let mut text = String::new();
        while let Some(e) = rx2.recv().await {
            if let AgentEvent::TextDelta { delta, .. } = e {
                text.push_str(&delta);
            }
        }
        text
    });
    let _ = run_agent_turn(
        req2,
        registry,
        ContextBundle::new("dd-2"),
        vec![provider2],
        tx2,
        Some(repo.clone()),
    )
    .await
    .expect("turn 2 (recall order after compaction)");
    let answer = pump2.await.unwrap();
    eprintln!("[judge_durable_verbatim_vs_droppable][{}] answer={:?}", ch.label, answer);
    assert!(!answer.is_empty(), "empty answer");

    // Deterministic guard: durable trading_write tool_result must survive INLINE verbatim with
    // both orderId AND fillPrice (spec §4: trading_write results are never stubbed/dropped).
    let all = repo.load_conversation(&conversation_id).unwrap();
    let durable_inline = all.iter().any(|m| {
        m.blocks.iter().any(|b| matches!(b, AgentMessageBlock::Text { text }
            if text.contains("tool_result") && text.contains(order_id) && text.contains(fill_price)))
    });
    assert!(
        durable_inline,
        "durable place_order tool_result (orderId {order_id} + fillPrice {fill_price}) must survive inline verbatim after compaction"
    );

    let scenario = format!(
        "After compaction, the user asked for the order id. The trading-write tool (place_order) had \
        returned orderId={order_id}, fillPrice={fill_price}. Trading-write results are never dropped \
        by compaction, so the agent must report the exact order id verbatim."
    );
    let rubric = format!(
        "1) 回答精确给出订单号 {order_id}(逐字)；\
        2) 没有声称忘记/不知道；3) 没有编造一个不同的订单号。"
    );
    let v = judge(&judge_ch, &scenario, &answer, &rubric)
        .await
        .unwrap_or_else(|e| panic!("judge error: {e}"));
    assert_verdict("durable_verbatim_vs_droppable", ch.label, &v);
}

// ===========================================================================
// ===== B. 多轮上下文管理 =====
// ===========================================================================

// ---------------------------------------------------------------------------
// B1. judge_microclear_then_answer_correct — 一个 droppable tool(get_news)返回较大数据 →
//     压缩时被折成 stub → 后续问该数据 → agent 仍答对(要么 summary 保留要点，要么 re-use_tool
//     重新拉取)。Deterministic guard: 该 tool_result 在持久化里已被 stub 化(<tool_result_stub）。
//     [矩阵 B1]
// ---------------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn judge_microclear_then_answer_correct() {
    let Some(judge_ch) = judge_channel() else {
        eprintln!("[judge_microclear_then_answer_correct] skipped: channels not configured (JUDGE_*)");
        return;
    };
    let Some(ch) = pick_fast_agent_channel() else {
        eprintln!("[judge_microclear_then_answer_correct] skipped: channels not configured (agent)");
        return;
    };

    let repo = fresh_repo();
    let registry = Arc::new(ToolRegistry::new_without_persist());
    let conversation_id = "judge-microclear-conv".to_string();

    // Droppable tool returning a sizeable payload whose key fact is a headline.
    let headline = "比亚迪5月新能源车销量同比增长35%";
    let news_spec = ToolSpec::new(
        "get_news",
        "拉取某标的的最新新闻(droppable，可被压缩后重新拉取)。需要新闻时调用，并可再次调用刷新。",
        json!({"type":"object","properties":{"tsCode":{"type":"string"}},"required":["tsCode"]}),
        vec![r#"<use_tool name="get_news">{"tsCode":"002594.SZ"}</use_tool>"#.to_string()],
        5000,
        SideEffect::None,
    );
    let hl = headline.to_string();
    let news_handler: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(move |_inv: ToolInvocation| {
        let hl = hl.clone();
        Box::pin(async move {
            ToolHandlerOutput::ok(json!({
                "headline": hl,
                "body": "正文很长，仅作占位，反复重复以撑大体积。".repeat(8),
            }))
        }) as ToolHandlerFuture
    }));
    registry.register_tool(news_spec, news_handler).unwrap();

    // Turn 1: fetch the news (droppable big payload lands inline).
    // New contract: hand only the new user message; the engine persists it + the produced turn.
    let provider1 = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req1 = AgentRunRequest {
        run_id: "mc-1".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 4,
        input: vec![user_message(
            "mc-1",
            "请用 get_news 拉取 002594.SZ 的最新新闻，并把头条标题告诉我。",
        )],
        conversation_id: Some(conversation_id.clone()),
        compaction: None,
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
    };
    let (tx1, mut rx1) = mpsc::channel::<AgentEvent>(256);
    let pump1 = tokio::spawn(async move {
        let mut tools: Vec<(String, bool)> = Vec::new();
        while let Some(e) = rx1.recv().await {
            if let AgentEvent::ToolEnd { name, is_error, .. } = e {
                tools.push((name, is_error));
            }
        }
        tools
    });
    let _ = run_agent_turn(
        req1,
        registry.clone(),
        ContextBundle::new("mc-1"),
        vec![provider1],
        tx1,
        Some(repo.clone()),
    )
    .await
    .expect("turn 1 (get_news)");
    let t1 = pump1.await.unwrap();
    assert!(t1.iter().any(|(n, e)| n == "get_news" && !e), "[{}] get_news not dispatched", ch.label);

    // Turn 2: force MicroClear (keep_recent=1 so the older get_news tool_result is OUTSIDE the
    // tail window → stubbed), then ask about the headline again. New contract: hand only the
    // follow-up; engine persists + loads.
    let provider2 = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req2 = AgentRunRequest {
        run_id: "mc-2".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 4,
        // No summarize_prompt: stay in MicroClear/Drop lane so the droppable tool_result is
        // stubbed (not folded into a summary).
        input: vec![user_message("mc-2", "刚才那条新闻的头条标题是什么？如果需要可以再次拉取。")],
        conversation_id: Some(conversation_id.clone()),
        compaction: Some(tight_compaction(1, None, None)),
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
    };
    let (tx2, mut rx2) = mpsc::channel::<AgentEvent>(256);
    let pump2 = tokio::spawn(async move {
        let mut text = String::new();
        let mut saw_microclear = false;
        while let Some(e) = rx2.recv().await {
            match e {
                AgentEvent::TextDelta { delta, .. } => text.push_str(&delta),
                AgentEvent::Compacted { tier, .. } => {
                    if matches!(tier, crate::domain::agent::CompactedTier::MicroClear) {
                        saw_microclear = true;
                    }
                }
                _ => {}
            }
        }
        (text, saw_microclear)
    });
    let _ = run_agent_turn(
        req2,
        registry,
        ContextBundle::new("mc-2"),
        vec![provider2],
        tx2,
        Some(repo.clone()),
    )
    .await
    .expect("turn 2 (recall after microclear)");
    let (answer, saw_microclear) = pump2.await.unwrap();
    eprintln!(
        "[judge_microclear_then_answer_correct][{}] saw_microclear={saw_microclear} answer={:?}",
        ch.label, answer
    );
    assert!(!answer.is_empty(), "empty answer");

    let scenario = format!(
        "A droppable news tool earlier returned a large payload whose headline was: '{headline}'. The \
        context was compacted (MicroClear stubbed the bulky tool_result). The user asked for the \
        headline again — the agent may re-fetch via the tool. The answer must still convey the headline."
    );
    let rubric = format!(
        "1) 回答里的头条标题与原始头条一致或语义等价(原始：{headline})；\
        2) 没有编造一条与原始不同的新闻；3) 回答使用中文。"
    );
    let v = judge(&judge_ch, &scenario, &answer, &rubric)
        .await
        .unwrap_or_else(|e| panic!("judge error: {e}"));
    assert_verdict("microclear_then_answer_correct", ch.label, &v);
}

// ---------------------------------------------------------------------------
// B2. judge_summary_no_hallucination — 触发摘要后，judge 检查摘要既覆盖关注标的/已建判断/
//     未决问题/风险纪律，又**不编造**对话里没出现的事实(如杜撰的具体点位/未提及的标的)。
//     这是对 #4 summary_faithfulness 的幻觉面补强(显式给一个诱导编造的缺口)。
//     [矩阵 B2]
// ---------------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn judge_summary_no_hallucination() {
    let Some(judge_ch) = judge_channel() else {
        eprintln!("[judge_summary_no_hallucination] skipped: channels not configured (JUDGE_*)");
        return;
    };
    let Some(ch) = pick_fast_agent_channel() else {
        eprintln!("[judge_summary_no_hallucination] skipped: channels not configured (agent)");
        return;
    };

    let repo = fresh_repo();
    let conversation_id = "judge-nohall-conv".to_string();

    // The pending question (招行买入点位未定) is a deliberate "gap" the summarizer must NOT fill in
    // with an invented number.
    let original_facts = "\
user: 我关注招商银行(600036.SH)，想逢低分批买入。\n\
assistant: 好的，已记下关注招商银行、逢低分批的想法。\n\
user: 但是具体的买入点位我还没想清楚，先放着，是个未决问题。\n\
assistant: 明白，招行买入点位未定，列为未决问题。\n\
user: 我的风险纪律是单票仓位不超过20%，不做两融。";
    let seed = vec![
        conv_chat(&conversation_id, 0, AgentMessageRole::User, "我关注招商银行(600036.SH)，想逢低分批买入。"),
        conv_chat(&conversation_id, 1, AgentMessageRole::Assistant, "好的，已记下关注招商银行、逢低分批的想法。"),
        conv_chat(&conversation_id, 2, AgentMessageRole::User, "但是具体的买入点位我还没想清楚，先放着，是个未决问题。"),
        conv_chat(&conversation_id, 3, AgentMessageRole::Assistant, "明白，招行买入点位未定，列为未决问题。"),
        conv_chat(&conversation_id, 4, AgentMessageRole::User, "我的风险纪律是单票仓位不超过20%，不做两融。"),
    ];
    for m in &seed {
        repo.upsert_message(m).unwrap();
    }

    let summarize_prompt = "你是会话压缩器。请用中文把对话压缩成要点摘要，覆盖：关注标的、已建立的判断、\
        未决问题、风险纪律、用户偏好。只能基于对话里真实出现过的信息，**绝不允许补全或编造**任何对话里\
        没有给出的具体数值或事实(例如未给出的买入点位)。只输出摘要正文。";
    let fired = force_one_summarize_cycle(
        &repo,
        &ch.channel,
        &conversation_id,
        "nh-run",
        "请基于以上继续。",
        summarize_prompt,
        Some(judge_ch.clone()),
    )
    .await;
    let summary_text = summary_text_of(&repo, &conversation_id);
    eprintln!(
        "[judge_summary_no_hallucination][{}] fired={fired} summary={:?}",
        ch.label, summary_text
    );
    assert!(!summary_text.trim().is_empty(), "no Summary checkpoint produced");

    let scenario = format!(
        "An agent compressed this conversation into a summary. The ORIGINAL conversation was:\n{original_facts}\n\n\
        Critically, the buy price level for 招行 was explicitly LEFT UNDECIDED (a pending question). \
        A faithful summary must capture the facts WITHOUT inventing any value not present in the original."
    );
    let rubric = "1) 摘要覆盖关注标的(招行600036.SH)、逢低分批的判断、风险纪律(单票≤20%、不做两融)；\
        2) 摘要把『招行买入点位』标为未决/未定，而不是编造出一个具体点位数字；\
        3) 摘要没有引入任何原对话里没有出现的标的/数值/事实(无幻觉)。";
    let v = judge(&judge_ch, &scenario, &summary_text, rubric)
        .await
        .unwrap_or_else(|e| panic!("judge error: {e}"));
    assert_verdict("summary_no_hallucination", ch.label, &v);
}

// ---------------------------------------------------------------------------
// B3. judge_keep_recent_verbatim — 最近一轮里给一个很具体的数字(止损价 36.78)，强制对更早历史
//     摘要(keep_recent=1 保住最近轮逐字)，随后追问该具体数字 → agent 能逐字引用。
//     Deterministic guard: 含 36.78 的最近 user 消息原文仍在 view 里(未被摘要吃掉)。
//     [矩阵 B3]
// ---------------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn judge_keep_recent_verbatim() {
    let Some(judge_ch) = judge_channel() else {
        eprintln!("[judge_keep_recent_verbatim] skipped: channels not configured (JUDGE_*)");
        return;
    };
    let Some(ch) = pick_fast_agent_channel() else {
        eprintln!("[judge_keep_recent_verbatim] skipped: channels not configured (agent)");
        return;
    };

    let repo = fresh_repo();
    let conversation_id = "judge-keeprecent-conv".to_string();

    // Older history (will be summarized) + a recent turn carrying a very specific number.
    let seed = vec![
        conv_chat(&conversation_id, 0, AgentMessageRole::User, "我们先聊点别的：我关注新能源板块整体走势。"),
        conv_chat(&conversation_id, 1, AgentMessageRole::Assistant, "好的，已记下你关注新能源板块整体走势。"),
        conv_chat(&conversation_id, 2, AgentMessageRole::User, "另外我对宁德时代也有兴趣，先观察基本面。"),
        conv_chat(&conversation_id, 3, AgentMessageRole::Assistant, "明白，宁德时代先观察基本面。"),
        // Most-recent substantive user turn with the precise number to be quoted verbatim.
        conv_chat(&conversation_id, 4, AgentMessageRole::User, "重点记一下：我给比亚迪(002594.SZ)设的止损价是 36.78 元。"),
        conv_chat(&conversation_id, 5, AgentMessageRole::Assistant, "收到，比亚迪止损价 36.78 元，已记下。"),
    ];
    for m in &seed {
        repo.upsert_message(m).unwrap();
    }

    // keep_recent=2 keeps the last two messages (the 36.78 user turn + assistant ack) verbatim;
    // older turns get summarized. Then ask for the exact stop-loss price. New contract: hand only
    // the new question — the engine persists it and loads the prior history fixture as context.
    let summarize_prompt = "你是会话压缩器。请用中文把尾窗外的较早对话压缩成要点摘要。只输出摘要正文。";
    let provider = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req = AgentRunRequest {
        run_id: "kr-q".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 1,
        input: vec![user_message("kr-q", "我给比亚迪设的止损价具体是多少？请逐字给出那个数字。")],
        conversation_id: Some(conversation_id.clone()),
        compaction: Some(tight_compaction(2, Some(summarize_prompt), Some(judge_ch.clone()))),
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
    };
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let pump = tokio::spawn(async move {
        let mut text = String::new();
        while let Some(e) = rx.recv().await {
            if let AgentEvent::TextDelta { delta, .. } = e {
                text.push_str(&delta);
            }
        }
        text
    });
    let _ = run_agent_turn(
        req,
        Arc::new(ToolRegistry::new_without_persist()),
        ContextBundle::new("kr-q"),
        vec![provider],
        tx,
        Some(repo.clone()),
    )
    .await
    .expect("keep_recent turn");
    let answer = pump.await.unwrap();
    eprintln!("[judge_keep_recent_verbatim][{}] answer={:?}", ch.label, answer);
    assert!(!answer.is_empty(), "empty answer");

    // Deterministic guard: the recent turn carrying 36.78 must still be present VERBATIM in the
    // post-compaction view (keep_recent kept it; not swallowed by the summary).
    let view = repo.load_conversation_view(&conversation_id).unwrap();
    let recent_inline = view.iter().any(|m| {
        m.kind != Some(MessageKind::Summary)
            && m.blocks.iter().any(|b| matches!(b, AgentMessageBlock::Text { text } if text.contains("36.78")))
    });
    assert!(
        recent_inline,
        "recent turn with 36.78 must remain inline verbatim (keep_recent window) — view: {view:?}"
    );

    let scenario = "The most recent turn stated a precise stop-loss price for 比亚迪: 36.78 元. Older \
        history was summarized but the recent window is kept verbatim. The user then asked for the exact \
        stop-loss number. The answer must quote it exactly.";
    let rubric = "1) 回答精确给出止损价 36.78(元)，逐字正确；\
        2) 没有给出一个不同的数字；3) 没有声称不知道。";
    let v = judge(&judge_ch, scenario, &answer, rubric)
        .await
        .unwrap_or_else(|e| panic!("judge error: {e}"));
    assert_verdict("keep_recent_verbatim", ch.label, &v);
}

// ---------------------------------------------------------------------------
// B4. judge_drop_degrade_preserves_durable — summarize_prompt=None 时压缩走 Drop-oldest；
//     droppable 旧轮被丢弃，但 durable(trading_write 的 place_order 结果)仍保留 inline；
//     最后提问 durable 订单号仍答对。Deterministic guard: 持久化里仍含完整 orderId 的
//     <tool_result>，且无任何 <tool_result> 形式的 summary 检查点(确认没走 Summarize)。
//     [矩阵 B4]
// ---------------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn judge_drop_degrade_preserves_durable() {
    let Some(judge_ch) = judge_channel() else {
        eprintln!("[judge_drop_degrade_preserves_durable] skipped: channels not configured (JUDGE_*)");
        return;
    };
    let Some(ch) = pick_fast_agent_channel() else {
        eprintln!("[judge_drop_degrade_preserves_durable] skipped: channels not configured (agent)");
        return;
    };

    let repo = fresh_repo();
    let registry = Arc::new(ToolRegistry::new_without_persist());
    let conversation_id = "judge-dropdeg-conv".to_string();

    let order_id = "ORD-44777";
    let spec = ToolSpec::new(
        "place_order",
        "在模拟账户下单买入某标的。下单后返回订单号(orderId)。当用户要求买入时调用。",
        json!({"type":"object","properties":{"tsCode":{"type":"string"},"side":{"type":"string"},"qty":{"type":"integer"}},"required":["tsCode"]}),
        vec![r#"<use_tool name="place_order">{"tsCode":"600036.SH","side":"buy","qty":200}</use_tool>"#.to_string()],
        5000,
        SideEffect::TradingWrite,
    );
    let oid = order_id.to_string();
    let handler: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(move |_inv: ToolInvocation| {
        let oid = oid.clone();
        Box::pin(async move { ToolHandlerOutput::ok(json!({"orderId": oid, "status": "filled"})) })
            as ToolHandlerFuture
    }));
    registry.register_tool(spec, handler).unwrap();

    // Turn 1: some droppable chatter (persisted fixture) THEN place an order (durable). New
    // contract: the chatter above is the persisted fixture; hand only the new order request —
    // the engine persists it + the produced turn (incl. the durable place_order result).
    for (seq, text) in [
        (0i64, "随便聊聊：我今天看了下大盘，没什么特别的。"),
    ] {
        repo.upsert_message(&conv_chat(&conversation_id, seq, AgentMessageRole::User, text)).unwrap();
        repo.upsert_message(&conv_chat(&conversation_id, seq + 1, AgentMessageRole::Assistant, "好的，了解。")).unwrap();
    }

    let provider1 = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req1 = AgentRunRequest {
        run_id: "dg-1".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 4,
        input: vec![user_message("dg-1", "请用 place_order 帮我以市价买入 200 股 600036.SH，下单后告诉我订单号。")],
        conversation_id: Some(conversation_id.clone()),
        compaction: None,
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
    };
    let (tx1, mut rx1) = mpsc::channel::<AgentEvent>(256);
    let pump1 = tokio::spawn(async move {
        let mut tools: Vec<(String, bool)> = Vec::new();
        while let Some(e) = rx1.recv().await {
            if let AgentEvent::ToolEnd { name, is_error, .. } = e {
                tools.push((name, is_error));
            }
        }
        tools
    });
    let _ = run_agent_turn(
        req1,
        registry.clone(),
        ContextBundle::new("dg-1"),
        vec![provider1],
        tx1,
        Some(repo.clone()),
    )
    .await
    .expect("turn 1 (place_order)");
    let t1 = pump1.await.unwrap();
    assert!(t1.iter().any(|(n, e)| n == "place_order" && !e), "[{}] place_order not dispatched", ch.label);

    // Turn 2: NO summarize_prompt → Summarize tier DEGRADES to Drop-oldest. Force compaction and
    // ask for the order id. Durable trading_write must survive the Drop. New contract: hand only
    // the follow-up; engine persists + loads.
    let provider2 = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req2 = AgentRunRequest {
        run_id: "dg-2".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 2,
        input: vec![user_message("dg-2", "我刚才那笔订单的订单号是多少？请直接回答订单号。")],
        conversation_id: Some(conversation_id.clone()),
        compaction: Some(tight_compaction(1, None, None)), // summarize_prompt = None → Drop lane
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
    };
    let (tx2, mut rx2) = mpsc::channel::<AgentEvent>(256);
    let pump2 = tokio::spawn(async move {
        let mut text = String::new();
        let mut saw_summarize = false;
        while let Some(e) = rx2.recv().await {
            match e {
                AgentEvent::TextDelta { delta, .. } => text.push_str(&delta),
                AgentEvent::Compacted { tier, .. } => {
                    if matches!(tier, crate::domain::agent::CompactedTier::Summarize) {
                        saw_summarize = true;
                    }
                }
                _ => {}
            }
        }
        (text, saw_summarize)
    });
    let _ = run_agent_turn(
        req2,
        registry,
        ContextBundle::new("dg-2"),
        vec![provider2],
        tx2,
        Some(repo.clone()),
    )
    .await
    .expect("turn 2 (recall after drop-degrade)");
    let (answer, saw_summarize) = pump2.await.unwrap();
    eprintln!(
        "[judge_drop_degrade_preserves_durable][{}] saw_summarize={saw_summarize} answer={:?}",
        ch.label, answer
    );
    assert!(!answer.is_empty(), "empty answer");
    assert!(!saw_summarize, "summarize_prompt=None must NOT trigger a Summarize tier (should degrade to Drop)");

    // Deterministic guard: durable trading_write tool_result with the orderId must survive inline
    // through the Drop-oldest degrade path (spec §4: Drop keeps durable items inline).
    let all = repo.load_conversation(&conversation_id).unwrap();
    let durable_inline = all.iter().any(|m| {
        m.blocks.iter().any(|b| matches!(b, AgentMessageBlock::Text { text }
            if text.contains("tool_result") && text.contains(order_id)))
    });
    assert!(
        durable_inline,
        "durable place_order tool_result (orderId {order_id}) must survive inline through Drop-degrade"
    );
    // And no summary checkpoint should have been produced (we never gave a summarize_prompt).
    assert_eq!(
        audit_summary_count(&repo, &conversation_id),
        0,
        "no Summary checkpoint should exist when summarize_prompt=None"
    );

    let scenario = format!(
        "With NO summarize prompt configured, compaction degrades to Drop-oldest. The trading-write \
        tool (place_order) had returned orderId={order_id}. Drop never removes durable trading-write \
        results. After compaction the user asked for the order id; the answer must still know it."
    );
    let rubric = format!(
        "1) 回答正确给出订单号 {order_id}；\
        2) 没有声称忘记/不知道；3) 没有编造一个不同的订单号。"
    );
    let v = judge(&judge_ch, &scenario, &answer, &rubric)
        .await
        .unwrap_or_else(|e| panic!("judge error: {e}"));
    assert_verdict("drop_degrade_preserves_durable", ch.label, &v);
}

// ---------------------------------------------------------------------------
// Harness unit tests (hermetic — these are NOT #[ignore]; they validate the JSON
// extraction / verdict parsing logic with no network).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod harness_unit {
    use super::{extract_json_object, parse_verdict};

    #[test]
    fn extract_plain_json_object() {
        let s = r#"{"pass": true, "score": 0.9, "reason": "ok"}"#;
        assert_eq!(extract_json_object(s).unwrap(), s);
    }

    #[test]
    fn extract_json_from_prose_and_fences() {
        let s = "Sure, here is my verdict:\n```json\n{\"pass\": false, \"score\": 0.2, \"reason\": \"off-topic\"}\n```\nDone.";
        let obj = extract_json_object(s).unwrap();
        assert_eq!(obj, r#"{"pass": false, "score": 0.2, "reason": "off-topic"}"#);
    }

    #[test]
    fn extract_handles_braces_inside_strings() {
        let s = r#"{"pass": true, "score": 1, "reason": "uses {curly} braces"}"#;
        assert_eq!(extract_json_object(s).unwrap(), s);
    }

    #[test]
    fn parse_verdict_full() {
        let v = parse_verdict(r#"{"pass": true, "score": 0.8, "reason": "good"}"#).unwrap();
        assert!(v.pass);
        assert!((v.score - 0.8).abs() < 1e-6);
        assert_eq!(v.reason, "good");
    }

    #[test]
    fn parse_verdict_defaults_score_when_missing() {
        let v = parse_verdict(r#"{"pass": false, "reason": "nope"}"#).unwrap();
        assert!(!v.pass);
        assert_eq!(v.score, 0.0);
    }

    #[test]
    fn parse_verdict_rejects_non_object() {
        assert!(parse_verdict("not json at all").is_none());
    }
}
