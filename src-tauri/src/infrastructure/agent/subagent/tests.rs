//! hermetic tests（无网络；ScriptedProvider）。原 subagent.rs 内联测试外移。

// ───────────────────────── tests (hermetic, no network) ─────────────────────────
use super::fork::spawn_or_run;
use super::*;
use crate::domain::agent::{
    AgentEvent, AgentMessage, AgentMessageBlock, AgentMessageRole, AgentRunRequest,
    AgentStopReason, ContextBundle, ProviderChannel, SideEffect, ToolSpec, WireFormat,
};
use crate::domain::agent::SubAgentActivityKind;
use crate::domain::shared::ErrorCode;
use chrono::Utc;
use crate::infrastructure::agent::loop_executor::{run_agent_turn_forked, LoopError, ProviderStream};
use crate::infrastructure::agent::skill_store::SkillStore;
use crate::infrastructure::agent::tool_registry::{
    ToolHandlerFuture, ToolHandlerOutput, ToolInvocation, ToolRegistry,
};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use uuid::Uuid;
use crate::infrastructure::agent::loop_executor::ProviderTurnOutcome;
use crate::infrastructure::agent::messages_repo::AgentMessagesRepo;
use crate::infrastructure::agent::migrations::migrations as agent_migrations;
use crate::infrastructure::agent::tool_parser::{ParserEvent, ToolCallParser};
use crate::infrastructure::agent::tool_registry::{FnToolHandler, ToolHandler};
use crate::infrastructure::db::{run_migrations, AppDb};
use tokio::sync::mpsc::Sender;

fn channel() -> ProviderChannel {
    ProviderChannel {
        channel_id: "fake".into(),
        provider: "fake".into(),
        wire_format: WireFormat::Messages,
        base_url: None,
        api_key: String::new(),
        model: "fake-model".into(),
        stream: true,
        enabled: true,
        supports_vision: false,
        supports_thinking: false,
        max_output_tokens: None,
        context_window_tokens: None,
        thinking_budget_tokens: None,
    }
}

fn scripted_outcome(text: &str, stop: AgentStopReason) -> ProviderTurnOutcome {
    let mut parser = ToolCallParser::new();
    let mut events = parser.feed(text);
    events.extend(parser.finalize());
    let tool_events: Vec<ParserEvent> = events
        .into_iter()
        .filter(|e| !matches!(e, ParserEvent::TextDelta(_)))
        .collect();
    ProviderTurnOutcome {
        text: text.to_string(),
        usage_input: 3,
        usage_output: 5,
        stop_reason: stop,
        tool_events,
    }
}

/// Scripted provider: replays one turn of fixed text. emit clean TextDelta (mirrors HttpProvider).
struct ScriptedProvider {
    outcome: Option<ProviderTurnOutcome>,
    delay_ms: u64,
}
#[async_trait::async_trait]
impl ProviderStream for ScriptedProvider {
    async fn next_turn(
        &mut self,
        _messages: &[AgentMessage],
        _context: &ContextBundle,
        event_tx: &Sender<AgentEvent>,
        run_id: &str,
    ) -> Result<ProviderTurnOutcome, LoopError> {
        if self.delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        }
        let out = self
            .outcome
            .take()
            .ok_or_else(|| LoopError::Provider("scripted exhausted".into()))?;
        let mut parser = ToolCallParser::new();
        let mut events = parser.feed(&out.text);
        events.extend(parser.finalize());
        for ev in events {
            if let ParserEvent::TextDelta(s) = ev {
                let _ = event_tx
                    .send(AgentEvent::TextDelta {
                        run_id: run_id.to_string(),
                        delta: s,
                    })
                    .await;
            }
        }
        Ok(out)
    }
}

/// A provider factory that hands out a fresh scripted provider replaying `text` (single turn).
fn fixed_factory(text: &'static str) -> ProviderFactory {
    Arc::new(move |_ch: &ProviderChannel| {
        Ok(Box::new(ScriptedProvider {
            outcome: Some(scripted_outcome(text, AgentStopReason::Completed)),
            delay_ms: 0,
        }) as Box<dyn ProviderStream>)
    })
}

fn slow_factory(text: &'static str, delay_ms: u64) -> ProviderFactory {
    Arc::new(move |_ch: &ProviderChannel| {
        Ok(Box::new(ScriptedProvider {
            outcome: Some(scripted_outcome(text, AgentStopReason::Completed)),
            delay_ms,
        }) as Box<dyn ProviderStream>)
    })
}

fn fresh_repo() -> AgentMessagesRepo {
    let db = AppDb::open_in_memory().unwrap();
    db.with(|c| run_migrations(c, agent_migrations()).unwrap());
    AgentMessagesRepo::new(db)
}

fn skill_store_with(name: &str, desc: &str, body: &str) -> (SkillStore, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("gangzi-subagent-skill-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = SkillStore::new(dir.clone());
    let md = store.skill_md_path(name);
    std::fs::create_dir_all(md.parent().unwrap()).unwrap();
    let content =
        crate::infrastructure::agent::skill_store::render_skill_md(name, desc, body);
    std::fs::write(&md, content).unwrap();
    (store, dir)
}

fn empty_skill_store() -> (SkillStore, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("gangzi-subagent-empty-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    (SkillStore::new(dir.clone()), dir)
}

fn base_handle(factory: ProviderFactory, repo: Option<AgentMessagesRepo>) -> ForkHandle {
    let (store, _dir) = empty_skill_store();
    ForkHandle::new(
        Arc::new(ToolRegistry::new_without_persist()),
        factory,
        repo,
        None,
        channel(),
        store,
        SubAgentTaskRegistry::new(),
    )
    .for_parent_run("parent-run", None)
}

/// A fork runtime for an initiating run, parent_run_id = "parent-run".
/// `is_subagent=false` = top-level (may fork); `true` = already a sub-agent (may NOT fork).
fn base_rt(is_subagent: bool) -> ForkRuntime {
    ForkRuntime::new(channel(), "parent-run").with_is_subagent(is_subagent)
}

// ---- run_forked_agent: returns the child's final assistant text ----
#[tokio::test]
async fn run_forked_agent_returns_child_final_text() {
    let handle = base_handle(fixed_factory("子 agent 的结论：买入 600519"), None);
    // register so run_forked_agent's terminal `finish` has a task to update.
    handle.tasks.register("sub_1", "parent-run", "", "analyze", None);
    let out = run_forked_agent(&handle, &base_rt(false), "sub_1", "去分析", None, None)
        .await
        .unwrap();
    assert!(out.contains("子 agent 的结论"));
    let (st, _pr, result) = handle.tasks.snapshot("sub_1").unwrap();
    assert_eq!(st, SubAgentStatus::Completed);
    assert!(result.unwrap().contains("子 agent 的结论"));
}

// ---- 前端可见性：子 run 活动经 parent_event_tx 转发为 SubAgentActivity ----
#[tokio::test]
async fn fork_forwards_subagent_activity_to_parent_event_tx() {
    let handle = base_handle(fixed_factory("子 agent 结论：建议关注 600519"), None);
    handle.tasks.register("sub_act", "parent-run", "", "analyze", None);
    let (ptx, mut prx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    // 顶层 rt（is_subagent=false）+ 注入父 event_tx = 前端通道。
    let rt = ForkRuntime::new(channel(), "parent-run").with_event_tx(Some(ptx));
    let out = run_forked_agent(&handle, &rt, "sub_act", "去调研", None, None)
        .await
        .unwrap();
    assert!(out.contains("结论"));

    let mut kinds = Vec::new();
    let mut saw_text = false;
    while let Ok(ev) = prx.try_recv() {
        if let AgentEvent::SubAgentActivity { run_id, agent_id, kind, text } = ev {
            assert_eq!(run_id, "parent-run", "活动须挂父 run_id（前端按它路由）");
            assert_eq!(agent_id, "sub_act");
            if matches!(kind, SubAgentActivityKind::Text) && !text.is_empty() {
                saw_text = true;
            }
            kinds.push(kind);
        }
    }
    assert!(kinds.contains(&SubAgentActivityKind::Started), "须转发 Started");
    assert!(kinds.contains(&SubAgentActivityKind::Done), "须转发 Done");
    assert!(saw_text, "须转发子 agent 文本活动（前端展示它在输出什么）");
}

// ---- isolation: child uses a brand-new fork conversation_id (parent linkage encoded) ----
#[tokio::test]
async fn child_uses_fresh_fork_conversation_id() {
    let repo = fresh_repo();
    let handle = base_handle(fixed_factory("done"), Some(repo.clone()));
    handle
        .tasks
        .register("sub_iso", "parent-run", "", "iso", None);
    run_forked_agent(&handle, &base_rt(false), "sub_iso", "hi", None, None)
        .await
        .unwrap();
    // The child's messages were persisted under a `fork:<parent>:...` conversation_id,
    // distinct from any parent conversation (isolation). Find them by run_id.
    let msgs = repo.load_messages_by_run("sub_iso").unwrap();
    assert!(!msgs.is_empty(), "child run must persist messages");
    let conv = msgs[0].conversation_id.clone().unwrap();
    assert!(
        conv.starts_with("fork:sub_iso:"),
        "child conversation_id must encode parent linkage, got {conv}"
    );
}

// ---- allowed_tools tightening: child registry only carries the subset ----
#[tokio::test]
async fn allowed_tools_tightens_child_registry() {
    // parent registry has two tools: keep + drop.
    let parent = Arc::new(ToolRegistry::new_without_persist());
    let mk = |name: &str| {
        ToolSpec::new(
            name,
            "t",
            serde_json::json!({"type":"object"}),
            vec![format!(r#"<use_tool name="{name}">{{}}</use_tool>"#)],
            5000,
            SideEffect::None,
        )
    };
    let handler: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(|inv: ToolInvocation| {
        Box::pin(async move { ToolHandlerOutput::ok(inv.input) }) as ToolHandlerFuture
    }));
    parent.register_tool(mk("keep"), handler.clone()).unwrap();
    parent.register_tool(mk("drop"), handler).unwrap();

    let (store, _d) = empty_skill_store();
    let handle = ForkHandle::new(
        parent.clone(),
        fixed_factory("done"),
        None,
        None,
        channel(),
        store,
        SubAgentTaskRegistry::new(),
    );
    // None → inherit full parent.
    let full = handle.child_registry(&handle.registry, None).unwrap();
    assert!(full.has_tool("keep") && full.has_tool("drop"));
    // Some(["keep"]) → only keep.
    let tight = handle
        .child_registry(&handle.registry, Some(&["keep".to_string()]))
        .unwrap();
    assert!(tight.has_tool("keep"));
    assert!(!tight.has_tool("drop"), "drop must be filtered out");
}

// ---- no nesting: an initiator that is already a sub-agent (is_subagent=true) is refused ----
// (boolean guard, aligned with Claude Code `isInForkChild`; no depth counting). ----
#[tokio::test]
async fn subagent_flag_refuses_nested_fork() {
    let handle = base_handle(fixed_factory("x"), None);
    // An initiator already inside a sub-agent (is_subagent=true) must NOT be able to fork —
    // the defensive guard refuses it (for when the spawn tools were somehow not stripped).
    let err = run_forked_agent(&handle, &base_rt(true), "sub_deep", "go", None, None)
        .await
        .expect_err("must refuse fork when initiator is already a sub-agent");
    match err {
        LoopError::Provider(msg) => assert!(
            msg.contains("nested sub-agent not allowed"),
            "got: {msg}"
        ),
        other => panic!("unexpected error: {other:?}"),
    }
    // The top-level initiator (is_subagent=false) is NOT refused.
    handle.tasks.register("sub_ok", "parent-run", "", "ok", None);
    run_forked_agent(&handle, &base_rt(false), "sub_ok", "go", None, None)
        .await
        .expect("a top-level agent's fork must be allowed");
}

// ---- ForkRuntime::child marks the derived runtime as a sub-agent (boolean, no depth) ----
#[tokio::test]
async fn fork_runtime_child_marks_is_subagent() {
    let rt = base_rt(false);
    assert!(!rt.is_subagent());
    let child = rt.child("sub_x");
    assert!(child.is_subagent());
    assert_eq!(child.parent_run_id, "sub_x");
    // A child stays a sub-agent (idempotent — there is no level counting).
    assert!(child.child("sub_y").is_subagent());
}

// ---- spawn_or_run preflight: a sub-agent initiator is rejected with invalid_input ----
#[tokio::test]
async fn spawn_or_run_rejects_subagent_initiator() {
    let handle = base_handle(fixed_factory("x"), None);
    let out = spawn_or_run(
        &handle,
        &base_rt(true), // already a sub-agent
        "desc",
        "go",
        None,
        None,
        false,
    )
    .await;
    assert!(out.is_error);
    assert_eq!(out.error_code, Some(ErrorCode::InvalidInput));
}

// ---- run_subagent tool: foreground blocks and returns result ----
#[tokio::test]
async fn run_subagent_foreground_returns_result() {
    let handle = base_handle(fixed_factory("前台结果 42"), None);
    let registry = ToolRegistry::new_without_persist();
    register_subagent_tools(&registry, handle.clone()).unwrap();
    assert!(registry.has_tool("run_subagent"));
    assert!(registry.has_tool("run_skill"));

    let out = registry
        .dispatch_tool_call(
            "parent-run",
            "tc_1".into(),
            "run_subagent",
            serde_json::json!({ "prompt": "算个数" }),
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{:?}", out.output_summary);
    assert!(out.output_summary["result"]
        .as_str()
        .unwrap()
        .contains("前台结果 42"));
    assert!(out.output_summary["agentId"].as_str().is_some());
}

// ---- run_subagent empty prompt → invalid_input ----
#[tokio::test]
async fn run_subagent_rejects_empty_prompt() {
    let handle = base_handle(fixed_factory("x"), None);
    let registry = ToolRegistry::new_without_persist();
    register_subagent_tools(&registry, handle).unwrap();
    let out = registry
        .dispatch_tool_call(
            "parent-run",
            "tc_e".into(),
            "run_subagent",
            serde_json::json!({ "prompt": "  " }),
        )
        .await
        .unwrap();
    assert!(out.is_error);
    assert_eq!(out.error_code, Some(ErrorCode::InvalidInput));
}

// ---- background spawn: returns agentId immediately + <task-notification> lands in the
//      parent run's shared queue on completion（spec §3.5：父 loop 注入下一轮，不走前端通道）----
#[tokio::test]
async fn run_subagent_background_notifies_on_completion() {
    let handle = base_handle(slow_factory("后台完成", 20), None);
    let registry = ToolRegistry::new_without_persist();
    register_subagent_tools(&registry, handle.clone()).unwrap();
    let shared = crate::infrastructure::agent::loop_executor::RunSharedState::new();
    let rt = base_rt(false).with_shared(shared.clone());

    let out = registry
        .dispatch_tool_call_with_ext(
            "parent-run",
            "tc_bg".into(),
            "run_subagent",
            serde_json::json!({ "prompt": "后台跑", "runInBackground": true }),
            Some(rt.into_ext()),
        )
        .await
        .unwrap();
    assert!(!out.is_error);
    assert_eq!(out.output_summary["background"], true);
    let agent_id = out.output_summary["agentId"].as_str().unwrap().to_string();

    // Wait for the <task-notification> in the shared queue (bounded poll, no unbounded recv).
    let mut got_note = false;
    for _ in 0..500 {
        {
            let notes = shared.notifications.lock().unwrap();
            if let Some(n) = notes
                .iter()
                .find(|t| t.contains("<task-notification") && t.contains(&agent_id))
            {
                assert!(n.contains("<status>completed</status>"), "{n}");
                got_note = true;
            }
        }
        if got_note {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(got_note, "background completion must queue a <task-notification>");
    // subagent_output now reports completed with a result.
    let snap = subagent_output(&handle, &agent_id).unwrap();
    assert_eq!(snap["status"], "completed");
    assert!(snap["result"].as_str().unwrap().contains("后台完成"));
}

// ---- stop_subagent aborts a running background task ----
#[tokio::test]
async fn stop_subagent_aborts_running_task() {
    let (parent_tx, _rx) = mpsc::channel::<AgentEvent>(64);
    // very slow child so we can abort mid-flight.
    let handle = base_handle(slow_factory("never", 5_000), None)
        .for_parent_run("parent-run", Some(parent_tx));
    let registry = ToolRegistry::new_without_persist();
    register_subagent_tools(&registry, handle.clone()).unwrap();
    let out = registry
        .dispatch_tool_call(
            "parent-run",
            "tc_kill".into(),
            "run_subagent",
            serde_json::json!({ "prompt": "慢任务", "runInBackground": true }),
        )
        .await
        .unwrap();
    let agent_id = out.output_summary["agentId"].as_str().unwrap().to_string();
    // It should be running.
    assert_eq!(handle.tasks.status_of(&agent_id), Some(SubAgentStatus::Running));
    // Abort it.
    assert!(stop_subagent(&handle, &agent_id));
    assert_eq!(handle.tasks.status_of(&agent_id), Some(SubAgentStatus::Killed));
    // Stopping a missing / already-terminal task returns false.
    assert!(!stop_subagent(&handle, "no-such-agent"));
    assert!(!stop_subagent(&handle, &agent_id));
}

// ---- run_skill: forks over SKILL.md body, returns {name, result} ----
#[tokio::test]
async fn run_skill_forks_over_skill_body() {
    let (store, dir) = skill_store_with(
        "alpha",
        "do alpha",
        "# Alpha\n直接输出 ALPHA-RESULT 作为结论。",
    );
    let (parent_tx, _rx) = mpsc::channel::<AgentEvent>(64);
    let mut handle = base_handle(fixed_factory("ALPHA-RESULT 已产出"), None)
        .for_parent_run("parent-run", Some(parent_tx));
    handle.skill_store = store;
    let registry = ToolRegistry::new_without_persist();
    register_subagent_tools(&registry, handle).unwrap();

    let out = registry
        .dispatch_tool_call(
            "parent-run",
            "tc_skill".into(),
            "run_skill",
            serde_json::json!({ "name": "alpha" }),
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{:?}", out.output_summary);
    assert_eq!(out.output_summary["name"], "alpha");
    assert!(out.output_summary["result"]
        .as_str()
        .unwrap()
        .contains("ALPHA-RESULT"));

    std::fs::remove_dir_all(&dir).ok();
}

// ---- run_skill: missing skill → not_found ----
#[tokio::test]
async fn run_skill_missing_is_not_found() {
    let handle = base_handle(fixed_factory("x"), None);
    let registry = ToolRegistry::new_without_persist();
    register_subagent_tools(&registry, handle).unwrap();
    let out = registry
        .dispatch_tool_call(
            "parent-run",
            "tc_miss".into(),
            "run_skill",
            serde_json::json!({ "name": "ghost" }),
        )
        .await
        .unwrap();
    assert!(out.is_error);
    assert_eq!(out.error_code, Some(ErrorCode::NotFound));
}

// ---- run_skill: invalid slug rejected ----
#[tokio::test]
async fn run_skill_rejects_invalid_name() {
    let handle = base_handle(fixed_factory("x"), None);
    let registry = ToolRegistry::new_without_persist();
    register_subagent_tools(&registry, handle).unwrap();
    let out = registry
        .dispatch_tool_call(
            "parent-run",
            "tc_bad".into(),
            "run_skill",
            serde_json::json!({ "name": "../escape" }),
        )
        .await
        .unwrap();
    assert!(out.is_error);
    assert_eq!(out.error_code, Some(ErrorCode::InvalidInput));
}

// ---- task registry lifecycle: register → running → finish(completed) ----
#[tokio::test]
async fn task_registry_lifecycle() {
    let reg = SubAgentTaskRegistry::new();
    reg.register("a1", "p", "conv", "desc", None);
    assert_eq!(reg.len(), 1);
    assert_eq!(reg.status_of("a1"), Some(SubAgentStatus::Running));
    reg.finish(
        "a1",
        SubAgentStatus::Completed,
        SubAgentProgress { tokens: 10, tool_uses: 1, duration_ms: 0 },
        Some("res".into()),
    );
    let (st, pr, result) = reg.snapshot("a1").unwrap();
    assert_eq!(st, SubAgentStatus::Completed);
    assert_eq!(pr.tokens, 10);
    assert_eq!(result.as_deref(), Some("res"));
    // notify flag is one-shot.
    assert!(reg.take_notify_flag("a1"));
    assert!(!reg.take_notify_flag("a1"));
}

// ---- parallel background spawns run concurrently and each notifies (shared queue) ----
#[tokio::test]
async fn parallel_background_spawns_each_notify() {
    // 新契约（spec §3.5）：完成通知进父 run 的 RunSharedState.notifications 队列
    // （由父 loop 注入下一轮），不再以 TextDelta 信封发 event_tx。
    let handle = base_handle(slow_factory("ok", 15), None);
    let registry = Arc::new(ToolRegistry::new_without_persist());
    register_subagent_tools(&registry, handle.clone()).unwrap();
    let shared = crate::infrastructure::agent::loop_executor::RunSharedState::new();
    let rt = base_rt(false).with_shared(shared.clone());
    let ext = Some(rt.into_ext());

    let n = 3;
    for _ in 0..n {
        let out = registry
            .dispatch_tool_call_with_ext(
                "parent-run",
                ToolRegistry::new_tool_call_id(),
                "run_subagent",
                serde_json::json!({ "prompt": "并发", "runInBackground": true }),
                ext.clone(),
            )
            .await
            .unwrap();
        assert_eq!(out.output_summary["background"], true);
    }
    // Wait for all notifications to land in the shared queue.
    for _ in 0..500 {
        if shared.notifications.lock().unwrap().len() >= n {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let notes = shared.notifications.lock().unwrap().clone();
    assert_eq!(notes.len(), n, "each background spawn must notify once");
    assert!(notes.iter().all(|t| t.contains("<task-notification")));
}

// ───────── No nesting: child has no fork tools; nested fork attempts are rejected ─────────

/// Provider that, on its FIRST turn of a run, emits `<use_tool name="run_subagent">` (attempting
/// to fork a nested child via real `<use_tool>` dispatch), then on subsequent turns emits plain
/// text to terminate. Each run gets a FRESH instance. Used to prove a child run CANNOT nest:
/// since the child registry has no `run_subagent`, the dispatch is rejected as an unregistered
/// tool and no second-level child is ever created.
struct NestingForkProvider {
    turn: u32,
}
#[async_trait::async_trait]
impl ProviderStream for NestingForkProvider {
    async fn next_turn(
        &mut self,
        _messages: &[AgentMessage],
        _context: &ContextBundle,
        event_tx: &Sender<AgentEvent>,
        run_id: &str,
    ) -> Result<ProviderTurnOutcome, LoopError> {
        self.turn += 1;
        let (text, stop) = if self.turn == 1 {
            (
                r#"<use_tool name="run_subagent">{"prompt":"go deeper"}</use_tool>"#.to_string(),
                AgentStopReason::ProviderStop,
            )
        } else {
            ("done".to_string(), AgentStopReason::Completed)
        };
        // Emit clean TextDelta like a real provider (XML suppressed).
        let mut parser = ToolCallParser::new();
        let mut events = parser.feed(&text);
        events.extend(parser.finalize());
        for ev in events {
            if let ParserEvent::TextDelta(s) = ev {
                let _ = event_tx
                    .send(AgentEvent::TextDelta {
                        run_id: run_id.to_string(),
                        delta: s,
                    })
                    .await;
            }
        }
        Ok(scripted_outcome(&text, stop))
    }
}

fn nesting_factory() -> ProviderFactory {
    Arc::new(|_ch: &ProviderChannel| {
        Ok(Box::new(NestingForkProvider { turn: 0 }) as Box<dyn ProviderStream>)
    })
}

/// No nesting (the core invariant): the child run's registry must NOT contain the spawn tools
/// `run_subagent` / `run_skill` (so the child's system prompt has no way to fork), while still
/// keeping other tools (e.g. `create_skill`, `read_file`). Additionally, when a scripted child
/// nonetheless emits `<use_tool name="run_subagent">`, the dispatch is rejected (unregistered
/// tool) and NO second-level child run is created — proving forks fan out only one level deep.
#[tokio::test]
async fn subagent_has_no_fork_tools_no_nesting() {
    // 1) Direct structural check on child_registry: spawn tools stripped, others kept.
    let parent = Arc::new(ToolRegistry::new_without_persist());
    let mk = |name: &str| {
        ToolSpec::new(
            name,
            "t",
            serde_json::json!({"type":"object"}),
            vec![format!(r#"<use_tool name="{name}">{{}}</use_tool>"#)],
            5000,
            SideEffect::None,
        )
    };
    let handler: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(|inv: ToolInvocation| {
        Box::pin(async move { ToolHandlerOutput::ok(inv.input) }) as ToolHandlerFuture
    }));
    // Parent carries spawn tools (marked is_spawn) + non-spawn tools.
    parent.register_tool(mk("run_subagent").spawn(), handler.clone()).unwrap();
    parent.register_tool(mk("run_skill").spawn(), handler.clone()).unwrap();
    parent.register_tool(mk("create_skill"), handler.clone()).unwrap();
    parent.register_tool(mk("read_file"), handler).unwrap();

    let (store, _d) = empty_skill_store();
    let handle = ForkHandle::new(
        parent.clone(),
        nesting_factory(),
        None,
        None,
        channel(),
        store,
        SubAgentTaskRegistry::new(),
    );

    // allowed=None → inherit all parent tools, but spawn tools are still stripped.
    let inherited = handle.child_registry(&handle.registry, None).unwrap();
    assert!(!inherited.has_tool("run_subagent"), "child must NOT carry run_subagent");
    assert!(!inherited.has_tool("run_skill"), "child must NOT carry run_skill");
    assert!(inherited.has_tool("create_skill"), "create_skill is not spawn — kept");
    assert!(inherited.has_tool("read_file"), "non-spawn tools are inherited");

    // allowed explicitly lists spawn tools → still stripped (cannot be re-granted).
    let tightened = handle
        .child_registry(&handle.registry, Some(&[
            "run_subagent".to_string(),
            "run_skill".to_string(),
            "read_file".to_string(),
        ]))
        .unwrap();
    assert!(!tightened.has_tool("run_subagent"), "spawn tool stripped even if allow-listed");
    assert!(!tightened.has_tool("run_skill"), "spawn tool stripped even if allow-listed");
    assert!(tightened.has_tool("read_file"), "allow-listed non-spawn tool kept");

    // 2) End-to-end: a top-level run forks ONE child. That child's loop emits
    //    `<use_tool name="run_subagent">`, but its registry has no such tool → rejected, no
    //    grandchild created. Exactly ONE child run is registered.
    let tasks = SubAgentTaskRegistry::new();
    let (store2, _dir2) = empty_skill_store();
    let registry = Arc::new(ToolRegistry::new_without_persist());
    let handle2 = ForkHandle::new(
        registry.clone(),
        nesting_factory(),
        None,
        None,
        channel(),
        store2,
        tasks.clone(),
    );
    register_subagent_tools(&registry, handle2.clone()).unwrap();

    // Top-level run (is_subagent=false) dispatches run_subagent once → forks a single child.
    let request = AgentRunRequest {
        run_id: "top".into(),
        trigger: "test".into(),
        channel: channel(),
        max_turns: 6,
        input: vec![AgentMessage {
            message_id: "am-top".into(),
            run_id: Some("top".into()),
            conversation_id: None,
            seq: None,
            kind: None,
            role: AgentMessageRole::User,
            blocks: vec![AgentMessageBlock::Text { text: "start".into() }],
            created_at: Utc::now(),
        }],
        conversation_id: None,
        compaction: None,
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
    };
    let top_rt = ForkRuntime::new(channel(), "top"); // is_subagent=false (top-level)
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(512);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let summary = run_agent_turn_forked(
        request,
        registry.clone(),
        ContextBundle::new("top"),
        vec![nesting_factory()(&channel()).unwrap()],
        tx,
        None,
        Some(top_rt.into_ext()),
        None,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let _ = drain.await;
    assert_eq!(summary.stop_reason, AgentStopReason::Completed);

    // Exactly ONE child run was registered. The child loop's attempt to dispatch run_subagent
    // hits an empty/stripped registry → no grandchild. Forks fan out only one level deep.
    assert_eq!(
        tasks.len(),
        1,
        "only the top-level agent forks (one child); the child cannot nest (got {} tasks)",
        tasks.len()
    );
}

/// fork uses the channel from the INJECTED ForkRuntime (the live run's channel), not the
/// ForkHandle's placeholder default. We assert this by injecting a runtime whose channel has a
/// distinctive model and having the provider factory record which channel it was built over.
#[tokio::test]
async fn fork_uses_injected_runtime_channel_not_placeholder() {
    use std::sync::Mutex as StdMutex;
    let seen_models: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let seen = seen_models.clone();
    let factory: ProviderFactory = Arc::new(move |ch: &ProviderChannel| {
        seen.lock().unwrap().push(ch.model.clone());
        Ok(Box::new(ScriptedProvider {
            outcome: Some(scripted_outcome("ok", AgentStopReason::Completed)),
            delay_ms: 0,
        }) as Box<dyn ProviderStream>)
    });
    let (store, _dir) = empty_skill_store();
    // ForkHandle's DEFAULT channel uses model "PLACEHOLDER-MODEL".
    let mut placeholder = channel();
    placeholder.model = "PLACEHOLDER-MODEL".into();
    let handle = ForkHandle::new(
        Arc::new(ToolRegistry::new_without_persist()),
        factory,
        None,
        None,
        placeholder,
        store,
        SubAgentTaskRegistry::new(),
    )
    .for_parent_run("parent-run", None);
    let registry = ToolRegistry::new_without_persist();
    register_subagent_tools(&registry, handle).unwrap();

    // Inject a ForkRuntime whose channel uses model "LIVE-MODEL".
    let mut live = channel();
    live.model = "LIVE-MODEL".into();
    let rt = ForkRuntime::new(live, "parent-run"); // is_subagent=false (top-level)
    let out = registry
        .dispatch_tool_call_with_ext(
            "parent-run",
            "tc_live".into(),
            "run_subagent",
            serde_json::json!({ "prompt": "用真实 channel 跑" }),
            Some(rt.into_ext()),
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{:?}", out.output_summary);

    let models = seen_models.lock().unwrap();
    assert!(
        models.iter().any(|m| m == "LIVE-MODEL"),
        "fork must build provider over the injected runtime channel, got {models:?}"
    );
    assert!(
        !models.iter().any(|m| m == "PLACEHOLDER-MODEL"),
        "fork must NOT use the ForkHandle placeholder channel, got {models:?}"
    );
}

// ───────── Added hermetic regressions for the 定型后 fork/skill invariants ─────────

/// Provider that emits `<use_tool name="run_subagent">` on its FIRST turn and captures every
/// `<tool_result>` / `<tool_error>` it is fed back on subsequent turns, then terminates. Used to
/// prove a CHILD run that tries to nest gets a `<tool_error code="invalid_input">` (unregistered
/// tool), not a grandchild — the end-to-end of the "no nesting" invariant (spec §3.5).
struct NestThenCaptureProvider {
    turn: u32,
    captured: Arc<Mutex<Vec<String>>>,
}
#[async_trait::async_trait]
impl ProviderStream for NestThenCaptureProvider {
    async fn next_turn(
        &mut self,
        messages: &[AgentMessage],
        _context: &ContextBundle,
        event_tx: &Sender<AgentEvent>,
        run_id: &str,
    ) -> Result<ProviderTurnOutcome, LoopError> {
        self.turn += 1;
        // Record any tool_result/tool_error text fed in via the latest user message.
        if let Some(last) = messages.last() {
            for b in &last.blocks {
                if let AgentMessageBlock::Text { text } = b {
                    if text.contains("<tool_error") || text.contains("<tool_result") {
                        self.captured.lock().unwrap().push(text.clone());
                    }
                }
            }
        }
        let (text, stop) = if self.turn == 1 {
            (
                r#"<use_tool name="run_subagent">{"prompt":"nest"}</use_tool>"#.to_string(),
                AgentStopReason::ProviderStop,
            )
        } else {
            ("done".to_string(), AgentStopReason::Completed)
        };
        let mut parser = ToolCallParser::new();
        let mut events = parser.feed(&text);
        events.extend(parser.finalize());
        for ev in events {
            if let ParserEvent::TextDelta(s) = ev {
                let _ = event_tx
                    .send(AgentEvent::TextDelta {
                        run_id: run_id.to_string(),
                        delta: s,
                    })
                    .await;
            }
        }
        Ok(scripted_outcome(&text, stop))
    }
}

/// No-nesting (end-to-end, child's view): when a child run emits `<use_tool name="run_subagent">`,
/// its stripped registry has no such tool, so the loop feeds back a `<tool_error
/// code="invalid_input">` (unregistered tool, spec §2 失败 code 归类) and NO grandchild is spawned.
/// This complements `subagent_has_no_fork_tools_no_nesting` (which checks task count) by asserting
/// the child actually receives the protocol-level rejection, so the model can self-correct.
/// Spec: agent-infra-module.md §3.5 (不嵌套) + §2 (未注册 tool → invalid_input).
#[tokio::test]
async fn child_nested_fork_attempt_yields_tool_error_invalid_input() {
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_factory = captured.clone();
    let factory: ProviderFactory = Arc::new(move |_ch: &ProviderChannel| {
        Ok(Box::new(NestThenCaptureProvider {
            turn: 0,
            captured: captured_for_factory.clone(),
        }) as Box<dyn ProviderStream>)
    });

    let tasks = SubAgentTaskRegistry::new();
    let (store, _dir) = empty_skill_store();
    let registry = Arc::new(ToolRegistry::new_without_persist());
    let handle = ForkHandle::new(
        registry.clone(),
        factory,
        None,
        None,
        channel(),
        store,
        tasks.clone(),
    )
    .for_parent_run("top", None);
    register_subagent_tools(&registry, handle.clone()).unwrap();

    // Top-level run forks ONE child (is_subagent=false). The child loop emits run_subagent.
    let request = AgentRunRequest {
        run_id: "top".into(),
        trigger: "test".into(),
        channel: channel(),
        max_turns: 6,
        input: vec![AgentMessage {
            message_id: "am-top".into(),
            run_id: Some("top".into()),
            conversation_id: None,
            seq: None,
            kind: None,
            role: AgentMessageRole::User,
            blocks: vec![AgentMessageBlock::Text { text: "start".into() }],
            created_at: Utc::now(),
        }],
        conversation_id: None,
        compaction: None,
        fallback_channels: vec![],
        retry: None,
        token_budget: None,
    };
    let top_rt = ForkRuntime::new(channel(), "top");
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(512);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    run_agent_turn_forked(
        request,
        registry.clone(),
        ContextBundle::new("top"),
        vec![(handle_factory_for_top())(&channel()).unwrap()],
        tx,
        None,
        Some(top_rt.into_ext()),
        None,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let _ = drain.await;

    // Exactly one child run (no grandchild).
    assert_eq!(tasks.len(), 1, "only the top-level agent forks; child cannot nest");
    // The child was fed back a <tool_error code="invalid_input"> for its run_subagent attempt.
    let caps = captured.lock().unwrap();
    let joined = caps.join("\n");
    assert!(
        joined.contains("<tool_error") && joined.contains("run_subagent"),
        "child must receive a <tool_error> for its run_subagent attempt, got: {joined}"
    );
    assert!(
        joined.contains("invalid_input"),
        "the nested-fork tool_error must use code=invalid_input (unregistered tool), got: {joined}"
    );
}

/// Helper: the top-level provider for the end-to-end nesting test must itself emit run_subagent on
/// turn 1 (to fork the child), then finish. Reuses NestingForkProvider's shape but with its OWN
/// capture-free instance so the top run forks exactly one child.
fn handle_factory_for_top() -> ProviderFactory {
    Arc::new(|_ch: &ProviderChannel| {
        Ok(Box::new(NestingForkProvider { turn: 0 }) as Box<dyn ProviderStream>)
    })
}

/// Background completion notification must land in the **injected** ForkRuntime's shared
/// notification queue (the live run's RunSharedState), not the ForkHandle default. Proves the
/// run-time injection path (DispatchExt → ForkRuntime) drives the notification target.
/// Spec: agent-infra-module.md §3.5（fork 上下文 run 时注入 + 后台 <task-notification> 注入父下一轮）.
#[tokio::test]
async fn background_notification_uses_injected_runtime_event_tx() {
    // ForkHandle default has NO shared state (would drop notifications with a warn if used).
    let handle = base_handle(slow_factory("后台完成", 15), None);
    let registry = ToolRegistry::new_without_persist();
    register_subagent_tools(&registry, handle.clone()).unwrap();

    // Inject a ForkRuntime carrying the LIVE parent run's shared state.
    let shared = crate::infrastructure::agent::loop_executor::RunSharedState::new();
    let rt = ForkRuntime::new(channel(), "parent-run").with_shared(shared.clone());

    let out = registry
        .dispatch_tool_call_with_ext(
            "parent-run",
            "tc_bg_inj".into(),
            "run_subagent",
            serde_json::json!({ "prompt": "后台跑", "runInBackground": true }),
            Some(rt.into_ext()),
        )
        .await
        .unwrap();
    assert!(!out.is_error);
    let agent_id = out.output_summary["agentId"].as_str().unwrap().to_string();

    // The <task-notification> must arrive in the INJECTED shared queue.
    let mut got = false;
    for _ in 0..500 {
        {
            let notes = shared.notifications.lock().unwrap();
            if notes
                .iter()
                .any(|t| t.contains("<task-notification") && t.contains(&agent_id))
            {
                got = true;
            }
        }
        if got {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(got, "background completion must notify via the injected runtime's shared queue");
}

/// SubAgentTask.progress accumulates real wall-clock duration for a sub-run (spec §3.5 progress).
/// A deliberately slow scripted child guarantees a non-zero duration, so we can assert it is
/// recorded (the run-time `duration_ms` is measured, not a placeholder zero).
#[tokio::test]
async fn subagent_progress_records_nonzero_duration() {
    let handle = base_handle(slow_factory("done", 25), None);
    handle.tasks.register("sub_dur", "parent-run", "", "dur", None);
    run_forked_agent(&handle, &base_rt(false), "sub_dur", "go", None, None)
        .await
        .unwrap();
    let (_st, pr, _r) = handle.tasks.snapshot("sub_dur").unwrap();
    assert!(
        pr.duration_ms > 0,
        "sub-run progress must record a measured (non-zero) duration_ms, got {}",
        pr.duration_ms
    );
    // tokens accumulated from the child's usage (ScriptedProvider reports 3+5).
    assert!(pr.tokens > 0, "progress must accumulate child token usage");
}

/// Provider that, on turn 1, emits a natural-language *preamble* AND a `<use_tool name="read_file">`
/// (both must NOT leak to the parent: the XML mechanics for context hygiene, the preamble because
/// only the FINAL turn's text is returned), then on turn 2 emits the final answer.
struct NoisyThenFinalProvider {
    turn: u32,
}
#[async_trait::async_trait]
impl ProviderStream for NoisyThenFinalProvider {
    async fn next_turn(
        &mut self,
        _messages: &[AgentMessage],
        _context: &ContextBundle,
        event_tx: &Sender<AgentEvent>,
        run_id: &str,
    ) -> Result<ProviderTurnOutcome, LoopError> {
        self.turn += 1;
        let (text, stop) = if self.turn == 1 {
            // Turn 1: a natural-language preamble (PREAMBLE-LET-ME-READ) BEFORE a tool call. The
            // preamble is real authored text (a clean TextDelta) — it must be discarded because it
            // is NOT the final turn. The <use_tool> XML + read_file tool_result must also not leak.
            (
                r#"PREAMBLE-LET-ME-READ <use_tool name="read_file">{"path":"/etc/hosts"}</use_tool>"#.to_string(),
                AgentStopReason::ProviderStop,
            )
        } else {
            ("FINAL-SKILL-ANSWER".to_string(), AgentStopReason::Completed)
        };
        let mut parser = ToolCallParser::new();
        let mut events = parser.feed(&text);
        events.extend(parser.finalize());
        for ev in events {
            if let ParserEvent::TextDelta(s) = ev {
                let _ = event_tx
                    .send(AgentEvent::TextDelta {
                        run_id: run_id.to_string(),
                        delta: s,
                    })
                    .await;
            }
        }
        Ok(scripted_outcome(&text, stop))
    }
}

/// run_skill returns ONLY the child's **final-turn** text (spec §3.5 「只回末轮文本」, aligned with
/// Claude Code「最后一条消息」). This guards two things at once:
///  1. tool-call mechanics — the child's `<use_tool>` XML and the `read_file` tool_result payload
///     must not surface in the `result`; and
///  2. mid-turn preamble — the child's turn-1 natural-language preamble ("PREAMBLE-LET-ME-READ")
///     must be discarded, because it is NOT the final turn (the accumulator is cleared on every
///     `ToolStart`). Only turn 2's "FINAL-SKILL-ANSWER" is returned.
/// The SKILL.md body is the child's prompt (read via SkillStore), never inlined into the parent.
/// Spec: agent-infra-module.md §3.5 + §3.6.
#[tokio::test]
async fn run_skill_result_is_final_turn_text_only() {
    let (store, dir) = skill_store_with("beta", "do beta", "# Beta\n按流程产出。");
    let factory: ProviderFactory = Arc::new(|_ch: &ProviderChannel| {
        Ok(Box::new(NoisyThenFinalProvider { turn: 0 }) as Box<dyn ProviderStream>)
    });
    // Single parent registry carries read_file (inherited by the child so the child's
    // intermediate <use_tool name="read_file"> dispatches cleanly) — the handle inherits from it
    // and `register_subagent_tools` adds run_subagent/run_skill to the very same registry.
    let registry = Arc::new(ToolRegistry::new_without_persist());
    register_local_tools_for_test(&registry);
    let handle = ForkHandle::new(
        registry.clone(),
        factory,
        None,
        None,
        channel(),
        store,
        SubAgentTaskRegistry::new(),
    )
    .for_parent_run("parent-run", None);
    register_subagent_tools(&registry, handle).unwrap();

    let out = registry
        .dispatch_tool_call(
            "parent-run",
            "tc_skill_only".into(),
            "run_skill",
            serde_json::json!({ "name": "beta" }),
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{:?}", out.output_summary);
    let result = out.output_summary["result"].as_str().unwrap();
    assert!(
        result.contains("FINAL-SKILL-ANSWER"),
        "run_skill must return the child's final answer, got: {result}"
    );
    // The child's tool-call mechanics must not surface in the parent-visible result.
    assert!(
        !result.contains("<use_tool") && !result.contains("read_file"),
        "child's <use_tool> XML must NOT leak into the run_skill result, got: {result}"
    );
    assert!(
        !result.contains("<tool_result") && !result.contains("truncated"),
        "child's tool_result payload must NOT leak into the run_skill result, got: {result}"
    );
    // The child's turn-1 natural-language preamble must NOT appear: only the final turn is kept.
    assert!(
        !result.contains("PREAMBLE-LET-ME-READ"),
        "non-final-turn preamble must be discarded (only末轮 text returned), got: {result}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// Small test helpers to compose a registry carrying read_file for the run_skill test.
fn register_local_tools_for_test(registry: &ToolRegistry) {
    let handler: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(|_inv: ToolInvocation| {
        Box::pin(async move {
            ToolHandlerOutput::ok(serde_json::json!({ "content": "x", "truncated": false }))
        }) as ToolHandlerFuture
    }));
    let _ = registry.register_tool(
        ToolSpec::new(
            "read_file",
            "read",
            serde_json::json!({"type":"object"}),
            vec![r#"<use_tool name="read_file">{"path":"x"}</use_tool>"#.into()],
            5000,
            SideEffect::None,
        ),
        handler,
    );
}

/// run_subagent (foreground, multi-turn) returns ONLY the child's final-turn text. Turn 1 emits a
/// preamble + a read_file tool call (not the final turn → must be discarded); turn 2 emits the
/// answer. Mirrors `run_skill_result_is_final_turn_text_only` for the run_subagent path.
/// Spec: agent-infra-module.md §3.5 (只回末轮文本).
#[tokio::test]
async fn run_subagent_result_is_final_turn_text_only() {
    let factory: ProviderFactory = Arc::new(|_ch: &ProviderChannel| {
        Ok(Box::new(NoisyThenFinalProvider { turn: 0 }) as Box<dyn ProviderStream>)
    });
    let registry = Arc::new(ToolRegistry::new_without_persist());
    register_local_tools_for_test(&registry); // child inherits read_file for its turn-1 call.
    let handle = ForkHandle::new(
        registry.clone(),
        factory,
        None,
        None,
        channel(),
        empty_skill_store().0,
        SubAgentTaskRegistry::new(),
    )
    .for_parent_run("parent-run", None);
    register_subagent_tools(&registry, handle).unwrap();

    let out = registry
        .dispatch_tool_call(
            "parent-run",
            "tc_sub_final".into(),
            "run_subagent",
            serde_json::json!({ "prompt": "多轮跑，最后给结论" }),
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{:?}", out.output_summary);
    let result = out.output_summary["result"].as_str().unwrap();
    assert!(
        result.contains("FINAL-SKILL-ANSWER"),
        "run_subagent must return the child's final-turn answer, got: {result}"
    );
    assert!(
        !result.contains("PREAMBLE-LET-ME-READ"),
        "turn-1 preamble must be discarded (only末轮 text), got: {result}"
    );
    assert!(
        !result.contains("<use_tool") && !result.contains("read_file"),
        "tool-call mechanics must not leak, got: {result}"
    );
}

/// Provider that captures the text of the FIRST user message it is seeded with (so a test can
/// assert the fork system-prompt hint was injected), then emits a one-turn final answer.
struct CaptureSeedProvider {
    seen: Arc<Mutex<Vec<String>>>,
}
#[async_trait::async_trait]
impl ProviderStream for CaptureSeedProvider {
    async fn next_turn(
        &mut self,
        messages: &[AgentMessage],
        _context: &ContextBundle,
        event_tx: &Sender<AgentEvent>,
        run_id: &str,
    ) -> Result<ProviderTurnOutcome, LoopError> {
        if let Some(first) = messages.first() {
            for b in &first.blocks {
                if let AgentMessageBlock::Text { text } = b {
                    self.seen.lock().unwrap().push(text.clone());
                }
            }
        }
        let _ = event_tx
            .send(AgentEvent::TextDelta {
                run_id: run_id.to_string(),
                delta: "ok".to_string(),
            })
            .await;
        Ok(scripted_outcome("ok", AgentStopReason::Completed))
    }
}

/// The fork system-prompt hint (FORK_FINAL_MESSAGE_HINT) is injected into the child's seed message
/// on BOTH fork paths (run_subagent free prompt + run_skill SKILL.md), telling the child only its
/// last message is returned. Spec: agent-infra-module.md §3.5.
#[tokio::test]
async fn fork_injects_final_message_hint_into_child_prompt() {
    // ---- run_subagent path ----
    let seen_sub: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_sub_f = seen_sub.clone();
    let factory: ProviderFactory = Arc::new(move |_ch: &ProviderChannel| {
        Ok(Box::new(CaptureSeedProvider { seen: seen_sub_f.clone() }) as Box<dyn ProviderStream>)
    });
    let handle = base_handle(factory, None);
    handle.tasks.register("sub_hint", "parent-run", "", "hint", None);
    run_forked_agent(&handle, &base_rt(false), "sub_hint", "去分析 600519", None, None)
        .await
        .unwrap();
    let seed = seen_sub.lock().unwrap().join("\n");
    assert!(
        seed.contains(FORK_FINAL_MESSAGE_HINT),
        "run_subagent seed must carry the fork final-message hint, got: {seed}"
    );
    assert!(seed.contains("去分析 600519"), "the user prompt must still be present");

    // ---- run_skill path ----
    let seen_skill: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_skill_f = seen_skill.clone();
    let factory2: ProviderFactory = Arc::new(move |_ch: &ProviderChannel| {
        Ok(Box::new(CaptureSeedProvider { seen: seen_skill_f.clone() }) as Box<dyn ProviderStream>)
    });
    let (store, dir) = skill_store_with("gamma", "do gamma", "# Gamma\n产出结论。");
    let mut handle2 = base_handle(factory2, None);
    handle2.skill_store = store;
    let registry = ToolRegistry::new_without_persist();
    register_subagent_tools(&registry, handle2).unwrap();
    let out = registry
        .dispatch_tool_call(
            "parent-run",
            "tc_skill_hint".into(),
            "run_skill",
            serde_json::json!({ "name": "gamma" }),
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{:?}", out.output_summary);
    let seed2 = seen_skill.lock().unwrap().join("\n");
    assert!(
        seed2.contains(FORK_FINAL_MESSAGE_HINT),
        "run_skill seed must carry the fork final-message hint, got: {seed2}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ---- spec §3.6：allowedTools 含未注册名 → invalid_input（不静默忽略） ----
#[tokio::test]
async fn run_subagent_rejects_unknown_allowed_tools() {
    let handle = base_handle(fixed_factory("done"), None);
    let rt = base_rt(false);
    let inv = ToolInvocation {
        run_id: "parent-run".into(),
        tool_call_id: "tc_1".into(),
        name: "run_subagent".into(),
        input: serde_json::json!({
            "prompt": "去查行情",
            "allowedTools": ["fetch_quotes_typo_not_registered"]
        }),
    };
    let out = handle_run_subagent(handle, rt, inv).await;
    assert!(out.is_error, "{:?}", out.output_summary);
    assert_eq!(out.error_code, Some(ErrorCode::InvalidInput));
    let msg = out.output_summary["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("fetch_quotes_typo_not_registered"),
        "错误必须点名未注册工具: {msg}"
    );
}

// ---- spec §3.5：子 registry 继承 ForkRuntime.registry（父 per-run），而非全局 ----
#[tokio::test]
async fn child_inherits_per_run_registry_from_fork_runtime() {
    let handle = base_handle(fixed_factory("done"), None);
    // 父 run 的 per-run registry：带一个领域工具（全局 handle.registry 没有它）。
    let per_run = Arc::new(ToolRegistry::new_without_persist());
    per_run
        .register_tool(
            ToolSpec::new(
                "fetch_quotes",
                "领域工具",
                serde_json::json!({"type":"object"}),
                vec![r#"<use_tool name="fetch_quotes">{}</use_tool>"#.into()],
                5000,
                SideEffect::None,
            ),
            Arc::new(crate::infrastructure::agent::tool_registry::FnToolHandler(
                |inv: ToolInvocation| {
                    Box::pin(async move { ToolHandlerOutput::ok(inv.input) })
                        as crate::infrastructure::agent::tool_registry::ToolHandlerFuture
                },
            )),
        )
        .unwrap();
    let rt = base_rt(false).with_registry(per_run.clone());
    let child = handle
        .child_registry(handle.source_registry(&rt), None)
        .unwrap();
    assert!(
        child.has_tool("fetch_quotes"),
        "子 registry 必须继承父 per-run registry 的领域工具"
    );
    // 不带 ForkRuntime.registry → 退回全局（不含领域工具）。
    let rt2 = base_rt(false);
    let child2 = handle
        .child_registry(handle.source_registry(&rt2), None)
        .unwrap();
    assert!(!child2.has_tool("fetch_quotes"));
}

// ---- 父 cancel 传播给前台子 run ----
#[tokio::test]
async fn parent_cancel_propagates_to_child_run() {
    let handle = base_handle(fixed_factory("子结论"), None);
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel(); // 预先取消：子 loop 在第一个 turn 边界即停。
    let rt = base_rt(false).with_cancel(cancel);
    handle.tasks.register("sub_c", "parent-run", "", "x", None);
    let out = run_forked_agent(&handle, &rt, "sub_c", "go", None, None)
        .await
        .unwrap();
    assert!(out.is_empty(), "被取消的子 run 不应产出结论文本");
    let (st, _, _) = handle.tasks.snapshot("sub_c").unwrap();
    assert_eq!(st, SubAgentStatus::Killed, "cancelled 子 run 终态 = killed");
}

// ---- 后台完成通知进共享队列（不再经 TextDelta 污染前端） ----
#[tokio::test]
async fn background_notification_lands_in_shared_queue() {
    let handle = base_handle(fixed_factory("后台结论"), None);
    let shared = crate::infrastructure::agent::loop_executor::RunSharedState::new();
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    // 断言前端通道**没有**收到裸 TextDelta 信封（只有 SubAgentActivity）。
    let fe = tokio::spawn(async move {
        let mut bad = false;
        while let Some(ev) = rx.recv().await {
            if matches!(ev, AgentEvent::TextDelta { .. }) {
                bad = true;
            }
        }
        bad
    });
    let rt = base_rt(false)
        .with_shared(shared.clone())
        .with_event_tx(Some(tx.clone()));
    let out = spawn_or_run(&handle, &rt, "bg", "go", None, None, true).await;
    assert!(!out.is_error);
    let _agent_id = out.output_summary["agentId"].as_str().unwrap().to_string();
    // 等通知入队（终态在 finish() 先落、通知 push 在其后，按通知本身轮询免竞态）。
    for _ in 0..300 {
        if !shared.notifications.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    drop(tx);
    drop(rt);
    let saw_text_delta = fe.await.unwrap();
    assert!(!saw_text_delta, "通知不得以 TextDelta 信封发给前端");
    let notes = shared.notifications.lock().unwrap().clone();
    assert_eq!(notes.len(), 1, "完成通知必须进共享队列");
    assert!(notes[0].contains("<task-notification"), "{}", notes[0]);
    assert!(notes[0].contains("后台结论"), "通知带结论预览: {}", notes[0]);
}
