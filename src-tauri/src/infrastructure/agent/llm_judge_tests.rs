//! LLM-as-Judge live test suite for Agent Infra.
//!
//! Spec: docs/design/agent-infra-module.md §2 (Skill 协议 / durable facts) /
//!       §3 (Agent Loop) / §4 (上下文管理 / Summarize / compaction) / §5 (Infra Loop API).
//!
//! These tests are NOT deterministic script assertions. Each one drives a *real* agent-infra
//! capability live (run_agent_loop / run_agent_turn over a real HttpProvider), then asks a
//! separate JUDGE LLM to evaluate the produced output against a rubric, returning a structured
//! `{pass, score, reason}` verdict. The judge semantically validates the agent's core behaviors:
//! answer relevance, tool/skill faithfulness, multi-turn memory, summary faithfulness, and
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
    CompactionConfig, ContextBundle, MessageKind, ProviderChannel, SideEffect, SkillSpec,
    WireFormat,
};
use crate::infrastructure::agent::http_provider::HttpProvider;
use crate::infrastructure::agent::loop_executor::{
    run_agent_loop, run_agent_loop_with_deps, run_agent_turn, ProviderStream, RunAgentDeps,
};
use crate::infrastructure::agent::messages_repo::AgentMessagesRepo;
use crate::infrastructure::agent::skill_registry::{
    FnSkillHandler, SkillHandler, SkillHandlerFuture, SkillHandlerOutput, SkillInvocation,
    SkillRegistry,
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
// Run-loop helper: drive run_agent_loop and collect the streamed answer + skills
// ---------------------------------------------------------------------------

struct LoopRun {
    answer: String,
    skills: Vec<(String, bool)>,
    stop_reason: AgentStopReason,
}

/// Run a single-shot loop (one user question) over the given channel + registry; pump the event
/// stream and collect the streamed TextDelta answer + skill_end (name, is_error) pairs.
async fn run_loop_collect(
    channel: ProviderChannel,
    registry: Arc<SkillRegistry>,
    user_text: &str,
    max_turns: u32,
) -> LoopRun {
    let provider = Box::new(HttpProvider::new(channel.clone()).unwrap());
    let request = AgentRunRequest {
        run_id: "judge-run".into(),
        trigger: "user".into(),
        channel,
        max_turns,
        seed_messages: vec![user_message("judge-run", user_text)],
        conversation_id: None,
        compaction: None,
    };
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let pump = tokio::spawn(async move {
        let mut text = String::new();
        let mut skills: Vec<(String, bool)> = Vec::new();
        while let Some(e) = rx.recv().await {
            match e {
                AgentEvent::TextDelta { delta, .. } => text.push_str(&delta),
                AgentEvent::SkillEnd { name, is_error, .. } => skills.push((name, is_error)),
                _ => {}
            }
        }
        (text, skills)
    });
    let summary = run_agent_loop(request, registry, ContextBundle::new("judge-run"), provider, tx)
        .await
        .expect("loop run");
    let (answer, skills) = pump.await.unwrap();
    LoopRun { answer, skills, stop_reason: summary.stop_reason }
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
//    run run_agent_loop, judge on-topic / plausible / Chinese / addresses the question.
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
        let registry = Arc::new(SkillRegistry::new_without_persist());
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
// 2. skill_faithfulness — register get_quote returning a FIXED made-up price (1234.5);
//    prompt the model to use it + report the price; judge that the model reports 1234.5
//    from the tool and did NOT invent a different number.
// ===========================================================================
#[tokio::test]
#[ignore]
async fn judge_skill_faithfulness() {
    let Some(judge_ch) = judge_channel() else {
        println!("[judge_skill_faithfulness] skip: set JUDGE_*");
        return;
    };
    let Some(ch) = pick_fast_agent_channel() else {
        println!("[judge_skill_faithfulness] skip: set an agent channel (TEST_DS_*/TEST_ANT_*/TEST_OAI_*)");
        return;
    };

    let registry = Arc::new(SkillRegistry::new_without_persist());
    let spec = SkillSpec::new(
        "get_quote",
        "获取单只 A股标的的实时行情快照。需要报某标的现价时必须调用本 skill 获取，不要凭空编造价格。",
        json!({"type":"object","properties":{"tsCode":{"type":"string"}},"required":["tsCode"]}),
        vec![r#"<use_skill name="get_quote">{"tsCode":"600519.SH"}</use_skill>"#.to_string()],
        5000,
        SideEffect::None,
    );
    // FIXED made-up price — the only correct number the model can report.
    let handler: Arc<dyn SkillHandler> = Arc::new(FnSkillHandler(|_inv: SkillInvocation| {
        Box::pin(async move { SkillHandlerOutput::ok(json!({"price": "1234.5"})) }) as SkillHandlerFuture
    }));
    registry.register_skill(spec, handler).unwrap();

    let question = "请调用 get_quote 获取 600519.SH 的现价，然后用一句话告诉我它现在多少钱。";
    let run = run_loop_collect(ch.channel, registry, question, 4).await;
    println!(
        "[judge_skill_faithfulness][{}] stop={:?} skills={:?} answer={:?}",
        ch.label, run.stop_reason, run.skills, run.answer
    );
    assert!(
        run.skills.iter().any(|(n, err)| n == "get_quote" && !err),
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
    assert_verdict("skill_faithfulness", ch.label, &v);
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
    let registry = Arc::new(SkillRegistry::new_without_persist());
    let conversation_id = "judge-mem-conv".to_string();

    // Turn 1: user states the constraint. Persist the user seed ourselves (loop only persists
    // what it produces), then run the turn so the assistant reply also persists.
    let mut seed1 = vec![user_message("mem-1", "我的风险偏好是只买银行股，请记住这一点。")];
    seed1[0].conversation_id = Some(conversation_id.clone());
    seed1[0].seq = repo.next_seq(&conversation_id).ok();
    repo.upsert_message(&seed1[0]).unwrap();

    let provider1 = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req1 = AgentRunRequest {
        run_id: "mem-1".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 1,
        seed_messages: seed1,
        conversation_id: Some(conversation_id.clone()),
        compaction: None,
    };
    let (tx1, mut rx1) = mpsc::channel::<AgentEvent>(256);
    let pump1 = tokio::spawn(async move { while rx1.recv().await.is_some() {} });
    let _ = run_agent_turn(
        req1,
        registry.clone(),
        ContextBundle::new("mem-1"),
        provider1,
        tx1,
        RunAgentDeps::with_repo(repo.clone()),
    )
    .await
    .expect("turn 1");
    pump1.await.unwrap();

    // Turn 2: NEW run, same conversation_id, empty seed → run_agent_turn auto-loads the
    // compressed view (turn-1 history) as seed. Persist the turn-2 user message first.
    let mut follow = user_message("mem-2", "根据我之前告诉你的偏好，给我推荐一个值得关注的方向。");
    follow.conversation_id = Some(conversation_id.clone());
    follow.seq = repo.next_seq(&conversation_id).ok();
    repo.upsert_message(&follow).unwrap();

    // Load the view (incl. the just-persisted turn-2 user message) as explicit seed.
    let seed2 = repo.load_conversation_view(&conversation_id).unwrap();
    let provider2 = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req2 = AgentRunRequest {
        run_id: "mem-2".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 1,
        seed_messages: seed2,
        conversation_id: Some(conversation_id.clone()),
        compaction: None,
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
        provider2,
        tx2,
        RunAgentDeps::with_repo(repo.clone()),
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
    let registry = Arc::new(SkillRegistry::new_without_persist());
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
    let summarize_prompt = "你是会话压缩器。请用中文把以下对话压缩成一段要点摘要，必须覆盖以下要点：\
        关注标的、已建立的判断、未决问题、风险纪律、用户偏好。只输出摘要正文，不要添加额外解释。";
    let mut seed2 = seed.clone();
    seed2.push(user_message("sum-run2", "请基于以上信息继续。"));
    // Ensure the trailing message also has the conversation id + a fresh seq so it persists/ orders right.
    if let Some(last) = seed2.last_mut() {
        last.conversation_id = Some(conversation_id.clone());
        last.seq = Some(7);
    }

    let provider = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req = AgentRunRequest {
        run_id: "sum-run2".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 1,
        seed_messages: seed2,
        conversation_id: Some(conversation_id.clone()),
        compaction: Some(CompactionConfig {
            soft_limit_tokens: Some(1),
            summarize_threshold_tokens: Some(1),
            hard_limit_tokens: Some(1_000_000),
            keep_recent_turns: Some(1),
            summarize_prompt: Some(summarize_prompt.into()),
            compact_channel: None,
        }),
    };
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let pump = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    // 用带持久化的入口，Summarize 产出的 kind=Summary 检查点才会落到 repo（供下方读回判定）。
    let _ = run_agent_loop_with_deps(
        req,
        registry,
        ContextBundle::new("sum-run2"),
        provider,
        tx,
        RunAgentDeps::with_repo(repo.clone()),
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
    let registry = Arc::new(SkillRegistry::new_without_persist());
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
    let mut seed = history.clone();
    let mut q = user_message("cmp-2", "我前面告诉过你的模拟账户代号是多少？请直接回答那个代号。");
    q.conversation_id = Some(conversation_id.clone());
    q.seq = Some(4);
    seed.push(q);

    let summarize_prompt = "你是会话压缩器。请用中文把以下对话压缩成要点摘要，务必完整保留对话中提到的\
        所有关键事实（包括账户代号、关注标的等具体值）。只输出摘要正文。";

    let provider = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req = AgentRunRequest {
        run_id: "cmp-2".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 1,
        seed_messages: seed,
        conversation_id: Some(conversation_id.clone()),
        compaction: Some(CompactionConfig {
            soft_limit_tokens: Some(1),
            summarize_threshold_tokens: Some(1),
            hard_limit_tokens: Some(1_000_000),
            keep_recent_turns: Some(1),
            summarize_prompt: Some(summarize_prompt.into()),
            compact_channel: None,
        }),
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
    let _ = run_agent_loop(req, registry, ContextBundle::new("cmp-2"), provider, tx)
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
//    skill_result survived inline in the conversation messages (not stubbed/dropped).
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
    // SkillRegistry needs persistence for SkillCall audit; reuse the repo's db for a PayloadStore.
    let registry = Arc::new(SkillRegistry::new_without_persist());
    let order_id = "ORD-55123";
    let fill_price = "1801.0";
    let spec = SkillSpec::new(
        "place_order",
        "在模拟账户下单买入某标的。下单后会返回订单号(orderId)和成交价(fillPrice)。当用户要求买入某标的时调用本 skill。",
        json!({"type":"object","properties":{"tsCode":{"type":"string"},"side":{"type":"string"},"qty":{"type":"integer"}},"required":["tsCode"]}),
        vec![r#"<use_skill name="place_order">{"tsCode":"600519.SH","side":"buy","qty":100}</use_skill>"#.to_string()],
        5000,
        SideEffect::TradingWrite,
    );
    let oid = order_id.to_string();
    let fp = fill_price.to_string();
    let handler: Arc<dyn SkillHandler> = Arc::new(FnSkillHandler(move |_inv: SkillInvocation| {
        let oid = oid.clone();
        let fp = fp.clone();
        Box::pin(async move {
            SkillHandlerOutput::ok(json!({"orderId": oid, "fillPrice": fp, "status": "filled"}))
        }) as SkillHandlerFuture
    }));
    registry.register_skill(spec, handler).unwrap();

    let conversation_id = "judge-durable-conv".to_string();

    // Turn 1: ask the model to place an order (it must call place_order, getting the durable
    // orderId). Persist via run_agent_turn under the conversation.
    let mut seed1 = vec![user_message(
        "dur-1",
        "请用 place_order 帮我以市价买入 100 股 600519.SH，下单后告诉我订单号。",
    )];
    seed1[0].conversation_id = Some(conversation_id.clone());
    seed1[0].seq = repo.next_seq(&conversation_id).ok();
    repo.upsert_message(&seed1[0]).unwrap();

    let provider1 = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req1 = AgentRunRequest {
        run_id: "dur-1".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 4,
        seed_messages: seed1,
        conversation_id: Some(conversation_id.clone()),
        compaction: None,
    };
    let (tx1, mut rx1) = mpsc::channel::<AgentEvent>(256);
    let pump1 = tokio::spawn(async move {
        let mut skills: Vec<(String, bool)> = Vec::new();
        while let Some(e) = rx1.recv().await {
            if let AgentEvent::SkillEnd { name, is_error, .. } = e {
                skills.push((name, is_error));
            }
        }
        skills
    });
    let _ = run_agent_turn(
        req1,
        registry.clone(),
        ContextBundle::new("dur-1"),
        provider1,
        tx1,
        RunAgentDeps::with_repo(repo.clone()),
    )
    .await
    .expect("turn 1 (place_order)");
    let t1_skills = pump1.await.unwrap();
    println!("[judge_durable_fact_preserved][{}] turn1 skills={:?}", ch.label, t1_skills);
    assert!(
        t1_skills.iter().any(|(n, err)| n == "place_order" && !err),
        "[{}] place_order was not dispatched successfully",
        ch.label
    );

    // Turn 2: NEW run, same conversation, FORCE compaction, ask for the order id. Load the view as
    // seed, append + persist the follow-up question, and pass tiny thresholds + summarize_prompt.
    let mut follow = user_message("dur-2", "我刚才下的那笔订单的订单号是多少？请直接回答订单号。");
    follow.conversation_id = Some(conversation_id.clone());
    follow.seq = repo.next_seq(&conversation_id).ok();
    repo.upsert_message(&follow).unwrap();
    let seed2 = repo.load_conversation_view(&conversation_id).unwrap();

    let summarize_prompt = "你是会话压缩器。请用中文把以下对话压缩成要点摘要。只输出摘要正文。";
    let provider2 = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req2 = AgentRunRequest {
        run_id: "dur-2".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 2,
        seed_messages: seed2,
        conversation_id: Some(conversation_id.clone()),
        compaction: Some(CompactionConfig {
            soft_limit_tokens: Some(1),
            summarize_threshold_tokens: Some(1),
            hard_limit_tokens: Some(1_000_000),
            keep_recent_turns: Some(1),
            summarize_prompt: Some(summarize_prompt.into()),
            compact_channel: None,
        }),
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
        provider2,
        tx2,
        RunAgentDeps::with_repo(repo.clone()),
    )
    .await
    .expect("turn 2 (recall after compaction)");
    let (answer, saw_compaction) = pump2.await.unwrap();
    println!(
        "[judge_durable_fact_preserved][{}] saw_compaction={saw_compaction} answer={:?}",
        ch.label, answer
    );
    assert!(!answer.is_empty(), "empty answer");

    // Deterministic guard (spec §2/§4): the trading_write skill_result must survive inline in the
    // persisted conversation — it is a durable fact that compaction must never stub/drop. The full
    // <skill_result> text carrying the orderId must still be present in agent_messages.
    let all = repo.load_conversation(&conversation_id).unwrap();
    let order_id_inline = all.iter().any(|m| {
        m.blocks.iter().any(|b| match b {
            AgentMessageBlock::Text { text } => {
                text.contains("skill_result") && text.contains(order_id)
            }
            _ => false,
        })
    });
    assert!(
        order_id_inline,
        "trading_write skill_result with orderId {order_id} must survive inline (not stubbed/dropped) in persisted conversation"
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
    let registry = Arc::new(SkillRegistry::new_without_persist());
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

    let summarize_prompt = "你是会话压缩器。请产出一份完整的中文累积摘要：若输入里已有'前情摘要'，必须把它的内容与后续新对话合并，不得遗漏旧信息。必须覆盖：账户/标的、已建立判断、未决问题、风险纪律。只输出摘要正文。";
    let mut seed2 = seed.clone();
    seed2.push(mk(4, AgentMessageRole::User, Some(MessageKind::Chat), "请基于以上继续。"));

    let provider = Box::new(HttpProvider::new(ch.channel.clone()).unwrap());
    let req = AgentRunRequest {
        run_id: "roll".into(),
        trigger: "user".into(),
        channel: ch.channel.clone(),
        max_turns: 1,
        seed_messages: seed2,
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
    };
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let pump = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let _ = run_agent_loop_with_deps(
        req,
        registry,
        ContextBundle::new("roll"),
        provider,
        tx,
        RunAgentDeps::with_repo(repo.clone()),
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
