//! Run executor —— 把一次 trigger 跑成一个完整 AgentRun（WP2 脊梁）。
//!
//! Spec: docs/design/agent-runtime-module.md §2 闭环 / §6 编排流
//!
//! 流程：create_run（冻结策略版本）→ build_context（L1+L2+L3）→ run_agent_turn（Infra loop）
//!       → 排空 AgentEvent 流（转发前端 + 可观测）→ finish_run（按 stop_reason 落终态）。
//!
//! providers / registry 由调用方注入：生产从 channel 建 HttpProvider + 注册 BC handler（bootstrap）；
//! 测试注入 fake ProviderStream + 空/桩 registry。AnalysisResult/AgentTrade 记账由 tool handler
//! （operate_account / news 分析产出）在 run 内触发，不在本函数。

use std::sync::Arc;

use crate::domain::agent::channel::ProviderChannel;
use crate::domain::agent::loop_request::AgentRunRequest;
use crate::domain::agent::messages::AgentMessage;
use crate::domain::agent::runtime::{AgentRun, AgentRunStatus, AgentRunTrigger};
use crate::domain::agent::AgentStopReason;
use crate::infrastructure::agent::loop_executor::{
    run_agent_turn_forked, ProviderStream, RunSharedState,
};
use crate::infrastructure::agent::subagent::ForkRuntime;
use tokio_util::sync::CancellationToken;
use crate::infrastructure::agent::messages_repo::AgentMessagesRepo;
use crate::infrastructure::agent::tool_registry::ToolRegistry;
use crate::domain::agent::events::AgentEvent;

use super::context::{build_context, RealtimeSection};
use super::runs::RunService;
use super::strategy::StrategyService;
use super::wiring::{build_domain_registry_for_mode, RuntimeToolDeps};

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("db error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("loop error: {0}")]
    Loop(String),
}

/// 转发 LLM 流式事件给前端 / 可观测（生产注入；测试可省）。
pub type AgentEventSink = Arc<dyn Fn(AgentEvent) + Send + Sync>;

/// run_id → CancellationToken 表（cancel_agent_run 据此取消在跑的 run）。
pub type CancelRegistry = std::sync::Mutex<std::collections::HashMap<String, CancellationToken>>;

/// 在 Runtime 内建的领域 registry 上**追加注册** Infra 本地/skill/run_subagent 工具（bootstrap 注入）。
///
/// 领域 registry 由 execute_run 在 create_run 之后内建（绑定真 run_id + 冻结策略版本）；Infra
/// 工具与 fork（run_subagent）依赖运行期上下文，由 bootstrap 通过本 hook 在同一 registry 上补注册。
/// 带 `AgentRunMode`：review run 只读（spec §3 mode 表）——bootstrap 据此过滤写类 / spawn 类 Infra 工具。
///
/// 取 `&Arc<ToolRegistry>`：Infra 的 `register_local_tools` / fork 注册需要 Arc（handler 可能回持
/// registry 引用）。
pub type RegistryAugment =
    Arc<dyn Fn(&Arc<ToolRegistry>, crate::domain::agent::runtime::AgentRunMode) + Send + Sync>;

/// 一次 run 的执行入参（providers 由调用方按 channel 备好；registry 由 execute_run 内建）。
pub struct ExecuteRunParams {
    pub trigger: AgentRunTrigger,
    pub channel: ProviderChannel,
    pub providers: Vec<Box<dyn ProviderStream>>,
    /// 领域工具依赖（gateway + 服务）；execute_run 据此 + 真 run_id 内建 registry。
    pub deps: RuntimeToolDeps,
    /// bootstrap 在领域 registry 上追加 Infra 工具 / run_subagent 的 hook（可 None）。
    pub augment_registry: Option<RegistryAugment>,
    /// L3 实时上下文段（会下单的 run 必含「当日已下单意图」，由调用方保证）。
    pub realtime: Vec<RealtimeSection>,
    /// 本轮新消息（通常一条 user）。
    pub input: Vec<AgentMessage>,
    /// 多轮续接标识（dialogue 给会话 id；隔离单次 run 为 None）。
    pub conversation_id: Option<String>,
    pub max_turns: u32,
    /// review 被 fork 时指向父 run。
    pub parent_run_id: Option<String>,
    /// 持久化对话（多轮续接）；隔离/无状态可 None。
    pub repo: Option<AgentMessagesRepo>,
    /// 流式事件转发（前端）；可 None。
    pub event_sink: Option<AgentEventSink>,
    /// 取消令牌表（execute_run 登记本 run 的 token，cancel_agent_run 据此取消）；可 None。
    pub cancel_registry: Option<Arc<CancelRegistry>>,
    /// run token 预算护栏（spec §8/§11）：传给 Infra loop（`AgentRunRequest.tokenBudget`），
    /// 累计 input+output+子 run 回灌超此值 → `done(stop_reason=token_budget_exceeded)`。None = 不限。
    pub token_budget: Option<u32>,
    /// run 创建后、LLM turn 跑之前回调（拿到真 run_id）。news drain 用它 `mark_in_batch`
    /// 做 in-flight 保护 + 标 runId（spec §5）。可 None。
    pub on_run_created: Option<Box<dyn FnOnce(&str) + Send>>,
}

fn trigger_label(t: &AgentRunTrigger) -> &'static str {
    match t {
        AgentRunTrigger::UserChat { .. } => "user_chat",
        AgentRunTrigger::NewsBatch { .. } => "news_batch",
        AgentRunTrigger::AccountTrigger { .. } => "account_trigger",
        AgentRunTrigger::EodReview { .. } => "eod_review",
    }
}

/// stop_reason → run 终态。
///
/// `MaxTurns` **不**映射 Completed：跑满轮数说明任务很可能没真正收口（news 没 record_analysis /
/// account_trigger 没处置完），按 Completed 会让调用方误把 trigger 标 handled / news 标 analyzed。
/// 落 Failed + 明确 error 串，调用方按可恢复策略处理（重试 / 回 pending）。
fn status_for(stop: AgentStopReason) -> (AgentRunStatus, Option<String>) {
    match stop {
        AgentStopReason::Completed | AgentStopReason::ProviderStop => {
            (AgentRunStatus::Completed, None)
        }
        AgentStopReason::Cancelled => (AgentRunStatus::Cancelled, None),
        other => (
            AgentRunStatus::Failed,
            Some(format!("stop_reason={other:?}")),
        ),
    }
}

/// 执行一次 run，返回 (AgentRun, RunSummary 的终态信息)。
///
/// 端到端：create_run → context → run_agent_turn（事件并发排空）→ finish_run。
pub async fn execute_run(
    runs: &RunService,
    strategy: &StrategyService,
    params: ExecuteRunParams,
) -> Result<AgentRun, ExecError> {
    let ExecuteRunParams {
        trigger,
        channel,
        providers,
        deps,
        augment_registry,
        realtime,
        input,
        conversation_id,
        max_turns,
        parent_run_id,
        repo,
        event_sink,
        cancel_registry,
        token_budget,
        on_run_created,
    } = params;

    // 1) create_run —— 冻结 active 策略版本、落库 running、emit RunStarted。
    let run = runs.create_run(
        trigger.clone(),
        channel.provider.clone(),
        channel.wire_format,
        channel.model.clone(),
        strategy,
        parent_run_id.clone(),
    )?;

    // 1.1) run 创建后、turn 跑前回调（news drain：mark_in_batch 做 in-flight 保护 + 标 runId）。
    if let Some(cb) = on_run_created {
        cb(&run.run_id);
    }

    // 1.5) 内建领域 registry —— **必须在 create_run 之后**：operate_account handler 绑定真 run_id +
    //      冻结策略版本，AgentTrade / orderId→runId 才记到正确的 run（决策链脊梁，spec §2/§4）。
    //      Infra 本地/skill/run_subagent 工具由 bootstrap 经 augment hook 在同一 registry 上补注册。
    let registry = build_domain_registry_for_mode(
        run.mode,
        &run.trigger,
        &deps,
        &run.run_id,
        run.strategy_version,
    );
    if let Some(augment) = augment_registry.as_ref() {
        augment(&registry, run.mode);
    }

    // 2) build_context —— L1 角色 + L2 策略（冻结版本对应的 active）+ L3 实时。
    let active = strategy.active()?;
    let ctx = build_context(&run.run_id, run.mode, active.as_ref(), realtime);

    // 3) AgentRunRequest（token 预算执行在 Infra：spec §8——loop 在 turn 边界比对累计
    //    input+output+子 run 回灌，超限 → done(stop_reason=token_budget_exceeded)，不再借道 cancel）。
    let request = AgentRunRequest {
        run_id: run.run_id.clone(),
        trigger: trigger_label(&trigger).to_string(),
        channel: channel.clone(),
        max_turns,
        input,
        conversation_id,
        compaction: None,
        fallback_channels: vec![],
        retry: None,
        token_budget: token_budget.map(|t| crate::domain::agent::TokenBudget { run_tokens: t }),
    };

    // 3.5) 登记取消令牌（cancel_agent_run 据 run_id 取消）。
    let cancel = CancellationToken::new();
    if let Some(cr) = cancel_registry.as_ref() {
        if let Ok(mut g) = cr.lock() {
            g.insert(run.run_id.clone(), cancel.clone());
        }
    }

    // 4) 并发排空事件流（转发前端）。
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);

    // 2.5) ForkRuntime：sub-agent tool 在 dispatch 时读取真实上下文（spec §3.5）：
    // - event_tx（= 前端通道）→ fork 子 run 的活动 SubAgentActivity 转发前端（可见性）。
    // - registry → 子 agent 默认继承**本 run** 的 per-run 工具集（含领域工具）。
    // - shared → 子 usage 回灌父预算 + 后台完成通知注入父下一轮。
    // - cancel → 取消父 run 时传播给跑着的子 run。
    let shared = RunSharedState::new();
    let fork_rt = ForkRuntime::new(channel, &run.run_id)
        .with_event_tx(Some(tx.clone()))
        .with_registry(registry.clone())
        .with_shared(shared.clone())
        .with_cancel(cancel.clone())
        .with_max_turns(max_turns);
    let fork_ctx = Some(fork_rt.into_ext());
    let drainer = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if let Some(sink) = event_sink.as_ref() {
                sink(ev);
            }
        }
    });

    let summary = run_agent_turn_forked(
        request,
        registry,
        ctx,
        providers,
        tx,
        repo,
        fork_ctx,
        Some(shared),
        cancel,
    )
    .await
    .map_err(|e| ExecError::Loop(e.to_string()));

    let _ = drainer.await; // tx 已随 run_agent_turn 返回 drop → drainer 自然结束

    // 注销取消令牌（run 终态）。
    if let Some(cr) = cancel_registry.as_ref() {
        if let Ok(mut g) = cr.lock() {
            g.remove(&run.run_id);
        }
    }

    // 5) finish_run —— 按 stop_reason 落终态 + emit RunFinished。
    let mut run = run;
    match summary {
        Ok(s) => {
            let (status, error) = status_for(s.stop_reason);
            runs.finish_run(&run.run_id, status, error.clone())?;
            // 返回值反映最新终态（调用方据此分流，如 news drain 成功才 mark_analyzed）。
            run.status = status;
            run.error = error;
            run.ended_at = Some(chrono::Utc::now());
        }
        Err(e) => {
            runs.finish_run(&run.run_id, AgentRunStatus::Failed, Some(e.to_string()))?;
            return Err(e);
        }
    }

    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::channel::WireFormat;
    use crate::domain::agent::context::ContextBundle;
    use crate::domain::agent::runtime::{AccountResultRef, AgentRunStatus};
    use crate::infrastructure::agent::loop_executor::{LoopError, ProviderTurnOutcome};
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::agent::runtime_repo::AgentRuntimeRepo;
    use crate::infrastructure::agent::tool_parser::{ParserEvent, ToolCallParser};
    use crate::infrastructure::db::{run_migrations, AppDb};
    use crate::pipeline::agent_runtime::gateways::{
        AccountGateway, GatewayError, NewsGateway, OperateOutcome, QuotesGateway,
    };
    use crate::pipeline::agent_runtime::records::RecordService;
    use serde_json::{json, Value as JsonValue};
    use tokio::sync::mpsc::Sender;

    /// 全只读放行的桩 gateway（quotes/news/account 的 fetch 都返回空对象）。
    struct StubGw;
    #[async_trait::async_trait]
    impl QuotesGateway for StubGw {
        async fn fetch(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
    }
    #[async_trait::async_trait]
    impl NewsGateway for StubGw {
        async fn fetch(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
    }
    #[async_trait::async_trait]
    impl AccountGateway for StubGw {
        async fn fetch(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
        async fn operate(&self, _: JsonValue, _: &str, _: &str) -> OperateOutcome {
            OperateOutcome::from_result(AccountResultRef::default())
        }
        async fn update_watchlist(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
    }

    /// 接受下单的 account gateway：operate 返回 accepted=true + 稳定 orderId（验证订单索引落地）。
    struct OkAccountGw;
    #[async_trait::async_trait]
    impl AccountGateway for OkAccountGw {
        async fn fetch(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
        async fn operate(&self, _: JsonValue, _: &str, _: &str) -> OperateOutcome {
            OperateOutcome::from_result(AccountResultRef {
                accepted: true,
                order_id: Some("ord_1".into()),
                account_event_ids: vec!["ev_1".into()],
                ..Default::default()
            })
        }
        async fn update_watchlist(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
    }

    fn deps_with_account(
        repo: &Arc<AgentRuntimeRepo>,
        account: Arc<dyn AccountGateway>,
    ) -> RuntimeToolDeps {
        RuntimeToolDeps {
            quotes: Arc::new(StubGw),
            news: Arc::new(StubGw),
            account,
            strategy: Arc::new(StrategyService::new(repo.clone())),
            records: Arc::new(RecordService::new(repo.clone())),
            persist: None,
            operate_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// 把脚本原文（含 `<use_tool>`）过 parser → ProviderTurnOutcome（与真 provider 同路径）。
    fn scripted(text: &str, stop: AgentStopReason) -> ProviderTurnOutcome {
        let mut parser = ToolCallParser::new();
        let mut events = parser.feed(text);
        events.extend(parser.finalize());
        let tool_events = events
            .into_iter()
            .filter(|e| !matches!(e, ParserEvent::TextDelta(_)))
            .collect();
        ProviderTurnOutcome {
            text: text.to_string(),
            usage_input: 5,
            usage_output: 7,
            stop_reason: stop,
            tool_events,
        }
    }

    /// 多 turn 脚本化 provider（按序吐预设 outcome）。
    struct ScriptedProvider {
        script: Vec<ProviderTurnOutcome>,
        index: usize,
    }
    #[async_trait::async_trait]
    impl ProviderStream for ScriptedProvider {
        async fn next_turn(
            &mut self,
            _messages: &[AgentMessage],
            _context: &ContextBundle,
            _event_tx: &Sender<AgentEvent>,
            _run_id: &str,
        ) -> Result<ProviderTurnOutcome, LoopError> {
            if self.index >= self.script.len() {
                return Err(LoopError::Provider("script exhausted".into()));
            }
            let out = self.script[self.index].clone();
            self.index += 1;
            Ok(out)
        }
    }

    /// 最小 fake ProviderStream：返回一个 text-only Completed turn（不触发工具）。
    struct FakeProvider {
        text: String,
        stop: AgentStopReason,
        used: bool,
    }

    #[async_trait::async_trait]
    impl ProviderStream for FakeProvider {
        async fn next_turn(
            &mut self,
            _messages: &[AgentMessage],
            _context: &ContextBundle,
            _event_tx: &Sender<AgentEvent>,
            _run_id: &str,
        ) -> Result<ProviderTurnOutcome, LoopError> {
            if self.used {
                return Err(LoopError::Provider("script exhausted".into()));
            }
            self.used = true;
            Ok(ProviderTurnOutcome {
                text: self.text.clone(),
                usage_input: 5,
                usage_output: 7,
                stop_reason: self.stop,
                tool_events: vec![],
            })
        }
    }

    fn channel() -> ProviderChannel {
        ProviderChannel {
            channel_id: "c".into(),
            provider: "anthropic".into(),
            wire_format: WireFormat::Messages,
            base_url: None,
            api_key: String::new(),
            model: "claude".into(),
            stream: true,
            enabled: true,
            supports_vision: false,
            supports_thinking: false,
            max_output_tokens: None,
            context_window_tokens: None,
            thinking_budget_tokens: None,
        }
    }

    fn setup() -> (RunService, StrategyService, Arc<AgentRuntimeRepo>) {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        let repo = Arc::new(AgentRuntimeRepo::new(db));
        (RunService::new(repo.clone()), StrategyService::new(repo.clone()), repo)
    }

    #[tokio::test]
    async fn execute_run_end_to_end_completes_and_records() {
        let (runs, strat, repo) = setup();
        strat.seed_baseline_if_empty().unwrap();

        let params = ExecuteRunParams {
            trigger: AgentRunTrigger::NewsBatch { news_ids: vec!["n1".into()] },
            channel: channel(),
            providers: vec![Box::new(FakeProvider {
                text: "已分析本批 news，无值得交易的边际信息，no_action。".into(),
                stop: AgentStopReason::Completed,
                used: false,
            })],
            deps: deps_with_account(&repo, Arc::new(StubGw)),
            augment_registry: None,
            realtime: vec![RealtimeSection::new("本批 news", "1) …")],
            input: vec![],
            conversation_id: None,
            max_turns: 3,
            parent_run_id: None,
            repo: None,
            event_sink: None,
            cancel_registry: None,
            token_budget: None,
            on_run_created: None,
        };

        let run = execute_run(&runs, &strat, params).await.unwrap();
        // run 创建时冻结了 baseline v1。
        assert_eq!(run.strategy_version, Some(1));
        // 终态落库为 completed（finish_run）。
        let stored = repo.get_run(&run.run_id).unwrap().unwrap();
        assert_eq!(stored.status, AgentRunStatus::Completed);
        assert!(stored.ended_at.is_some());
        // 不再有 running run。
        assert_eq!(repo.list_running_runs().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn execute_run_forwards_events_to_sink() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (runs, strat, repo) = setup();
        strat.seed_baseline_if_empty().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let c2 = count.clone();

        let params = ExecuteRunParams {
            trigger: AgentRunTrigger::UserChat { message_id: "m1".into() },
            channel: channel(),
            providers: vec![Box::new(FakeProvider {
                text: "你好，我可以帮你分析行情。".into(),
                stop: AgentStopReason::Completed,
                used: false,
            })],
            deps: deps_with_account(&repo, Arc::new(StubGw)),
            augment_registry: None,
            realtime: vec![],
            input: vec![],
            conversation_id: None,
            max_turns: 3,
            parent_run_id: None,
            repo: None,
            event_sink: Some(Arc::new(move |_ev| {
                c2.fetch_add(1, Ordering::SeqCst);
            })),
            cancel_registry: None,
            token_budget: None,
            on_run_created: None,
        };

        let _run = execute_run(&runs, &strat, params).await.unwrap();
        // 至少收到 RunStart + Done 两类事件。
        assert!(count.load(Ordering::SeqCst) >= 2, "应转发若干 AgentEvent");
    }

    /// 竖切贯穿：fake provider 真发起 operate_account 工具调用 →
    /// executor 内建 registry（绑定真 run_id）→ OperateAccountHandler → gateway.operate →
    /// records 落 AgentTrade(submitting→settled) + orderId→runId 索引。
    ///
    /// 这是 §11 关键验收：证明 AgentTrade 记到**正确**的 run_id（决策链脊梁），且订单索引可反查。
    #[tokio::test]
    async fn execute_run_dispatches_operate_account_and_records_trade() {
        let (runs, strat, repo) = setup();
        strat.seed_baseline_if_empty().unwrap();

        // turn1：发起 operate_account；turn2：纯文本收尾（工具结果回灌后）。
        let script = vec![
            scripted(
                r#"<use_tool name="operate_account">{"accountInput":{"action":"place_order","tsCode":"600519.SH"},"reason":"价值开仓"}</use_tool>"#,
                AgentStopReason::ProviderStop, // 有 dispatch，stop 被忽略，继续下一 turn
            ),
            scripted("已完成开仓建仓。", AgentStopReason::Completed),
        ];

        let params = ExecuteRunParams {
            trigger: AgentRunTrigger::NewsBatch { news_ids: vec!["n1".into()] },
            channel: channel(),
            providers: vec![Box::new(ScriptedProvider { script, index: 0 })],
            deps: deps_with_account(&repo, Arc::new(OkAccountGw)),
            augment_registry: None,
            realtime: vec![],
            input: vec![],
            conversation_id: None,
            max_turns: 5,
            parent_run_id: None,
            repo: None,
            event_sink: None,
            cancel_registry: None,
            token_budget: None,
            on_run_created: None,
        };

        let run = execute_run(&runs, &strat, params).await.unwrap();

        // ① AgentTrade 已 settled（无悬挂 submitting）。
        assert!(repo.list_submitting_trades().unwrap().is_empty(), "trade 应已 settled");

        // ② orderId→runId 索引反查命中，且指向**本 run**（run_id 一致 = 决策链未断）。
        let (origin_run, _trade_id) = repo
            .find_run_by_order_id("ord_1")
            .unwrap()
            .expect("orderId 应已建索引");
        assert_eq!(origin_run, run.run_id, "trade 必须记在本 run_id 下");

        // ③ run 正常完成。
        assert_eq!(
            repo.get_run(&run.run_id).unwrap().unwrap().status,
            AgentRunStatus::Completed
        );
    }
}
