//! Runtime 调度 —— news buffer M/N 触发 + age-out 维护（tokio 后台任务）。
//!
//! Spec: docs/design/agent-runtime-module.md §5 news 机制 / §6 编排流 / §8 维护调度
//!
//! news buffer 消费循环（单线程、顺序 await → 无自并发，一批跑完才进下一 tick）：
//! 每 `base_tick`：① age-out 过期 pending；② 若 `pending≥M`（立即）或累计等待 ≥`N`（max_wait）
//! 且有 pending → drain 一批（newest-first）→ `run_news_batch` → 成功后 `mark_analyzed`。
//!
//! account_trigger 实时消费 + 收盘 review 定时由 adapter/事件桥另接（依赖 Account 事件 + 交易日历）；
//! 本模块先落最清晰的 news buffer 循环 + age-out。

use std::sync::Arc;
use std::time::Duration as StdDuration;

use tokio::sync::mpsc;

use super::orchestrator::RuntimeServices;

/// 调度句柄：drop 即停（stop sender 关闭 → 循环 select 收到 → break）。
pub struct RuntimeSchedulerHandle {
    _join: tokio::task::JoinHandle<()>,
    _stop: mpsc::Sender<()>,
}

/// 一次 news buffer drain 判定 + 执行。返回 `Some(run_id)` 表示本 tick 跑了一批。
///
/// `elapsed_secs`：自上次非空 drain 起累计的等待秒（caller 维护，用于 N=max_wait 触发）。
/// 触发后 caller 应把它清零。返回 `(triggered, run_id?)`。
pub async fn drain_news_buffer(
    services: &RuntimeServices,
    elapsed_secs: i64,
) -> (bool, Option<String>) {
    // ⓪ 自动分析开关门（spec §5：默认关闭 → 不消费、不 age-out）。
    if !services.news_auto_analysis_enabled() {
        return (false, None);
    }

    // ① age-out 过期 pending + emit 丢弃计数（spec §5「必须让用户看见丢了多少」）。
    services.age_out_news_buffer();

    let pending = match services.news_buffer.pending_count() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(target: "runtime.sched.news", error = %e, "pending_count failed");
            return (false, None);
        }
    };
    if pending == 0 {
        return (false, None);
    }

    // ② 触发条件：M（数量）立即，或累计等待达 N（max_wait）。
    let hit_m = services.news_buffer.should_trigger_now().unwrap_or(false);
    let hit_n = elapsed_secs >= services.news_buffer.config().max_wait_secs as i64;
    if !hit_m && !hit_n {
        return (false, None);
    }

    // ③ drain：在 agent.news_batch lock 下 take→mark_in_batch→run→成功 analyzed /
    //    可恢复回 pending / 不可恢复 dropped（spec §5/§8）。
    let run_id = services.drain_news_batch().await;
    (run_id.is_some(), run_id)
}

/// 起账户自驱 quote tick 调度循环（spec §6 行情/账户维护调度 + §8 兜底 cadence）。
///
/// 每 `tick`：对 `subscribed_codes ∪ core_indexes` 做 focused refresh（同步 final=true，亚秒级）→
/// rebuild_account_snapshot → evaluate_account_triggers 分页耗尽。subscribed 集合为空（空仓且无挂单
/// 且无自选）则跳过（零成本）。单线程顺序 await：一 tick 跑完才进下一 tick，无自并发。
///
/// cadence 取 `account_trigger_eval_interval_secs`（缺省 10s）——这是账户触发评估的**唯一** cadence
/// （旧 Account BC 60s 纯兜底 eval scheduler 已退役）；trigger 经 triggerId 幂等去重，重复评估只空转。
pub fn spawn_account_eval_tick_scheduler(
    services: Arc<RuntimeServices>,
) -> RuntimeSchedulerHandle {
    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
    let secs = services.account_trigger_eval_interval_secs().max(1);

    let join = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(StdDuration::from_secs(secs));
        ticker.tick().await; // 跳过首个立即 tick（给 setup 收尾）
        loop {
            tokio::select! {
                _ = stop_rx.recv() => break,
                _ = ticker.tick() => {
                    services.run_account_eval_tick().await;
                }
            }
        }
    });

    RuntimeSchedulerHandle {
        _join: join,
        _stop: stop_tx,
    }
}

/// 起 news buffer 调度循环。`base_tick` 为基础轮询间隔（建议 ≤ N，决定 M 命中后的最大延迟）。
pub fn spawn_news_buffer_scheduler(
    services: Arc<RuntimeServices>,
    base_tick: StdDuration,
) -> RuntimeSchedulerHandle {
    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
    let base_secs = base_tick.as_secs() as i64;

    let join = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(base_tick);
        ticker.tick().await; // 跳过首个立即 tick
        let mut elapsed: i64 = 0;

        loop {
            tokio::select! {
                _ = stop_rx.recv() => break,
                _ = ticker.tick() => {
                    // 收盘后自动复盘（CN ≥15:30 且当日未复盘）。
                    services.maybe_run_eod_review().await;

                    let pending = services.news_buffer.pending_count().unwrap_or(0);
                    if pending > 0 {
                        elapsed += base_secs;
                    } else {
                        elapsed = 0;
                    }
                    let (triggered, _run) = drain_news_buffer(&services, elapsed).await;
                    if triggered {
                        elapsed = 0;
                    }
                }
            }
        }
    });

    RuntimeSchedulerHandle {
        _join: join,
        _stop: stop_tx,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::channel::{ProviderChannel, WireFormat};
    use crate::domain::agent::context::ContextBundle;
    use crate::domain::agent::events::AgentEvent;
    use crate::domain::agent::messages::AgentMessage;
    use crate::domain::agent::runtime::AccountResultRef;
    use crate::domain::agent::AgentStopReason;
    use crate::infrastructure::agent::channels_repo::ProviderChannelsRepo;
    use crate::infrastructure::agent::loop_executor::{LoopError, ProviderStream, ProviderTurnOutcome};
    use crate::infrastructure::agent::messages_repo::AgentMessagesRepo;
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::agent::payload_store::PayloadStore;
    use crate::infrastructure::agent::runtime_repo::AgentRuntimeRepo;
    use crate::infrastructure::db::{run_migrations, AppDb};
    use crate::pipeline::agent_runtime::gateways::{
        AccountGateway, GatewayError, NewsGateway, OperateOutcome, QuotesGateway,
    };
    use crate::pipeline::agent_runtime::news_buffer::{NewsBufferConfig, NewsBufferService};
    use crate::pipeline::agent_runtime::orchestrator::{
        ProviderFactory, RuntimeServicesConfig,
    };
    use crate::pipeline::agent_runtime::records::RecordService;
    use crate::pipeline::agent_runtime::runs::RunService;
    use crate::pipeline::agent_runtime::strategy::StrategyService;
    use crate::pipeline::agent_runtime::triggers::TriggerRouter;
    use crate::pipeline::agent_runtime::wiring::RuntimeToolDeps;
    use chrono::Utc;
    use serde_json::{json, Value as JsonValue};
    use tokio::sync::mpsc::Sender;

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
            Ok(json!({"items": []}))
        }
    }
    #[async_trait::async_trait]
    impl AccountGateway for StubGw {
        async fn fetch(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({"orders": [], "positions": []}))
        }
        async fn operate(&self, _: JsonValue, _: &str, _: &str) -> OperateOutcome {
            OperateOutcome::from_result(AccountResultRef::default())
        }
        async fn update_watchlist(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
    }

    /// 两轮 provider：Turn 1 发 record_analysis tool call（使 news run 产出 AnalysisResult，
    /// 满足 spec §3 drain 校验），Turn 2 无工具直接完成。
    struct FakeProvider {
        turn: u32,
    }
    #[async_trait::async_trait]
    impl ProviderStream for FakeProvider {
        async fn next_turn(
            &mut self,
            _m: &[AgentMessage],
            _c: &ContextBundle,
            _tx: &Sender<AgentEvent>,
            _r: &str,
        ) -> Result<ProviderTurnOutcome, LoopError> {
            use crate::infrastructure::agent::tool_parser::ParserEvent;
            self.turn += 1;
            if self.turn == 1 {
                Ok(ProviderTurnOutcome {
                    text: r#"<use_tool name="record_analysis">{"kind":"no_action","summary":"观望"}</use_tool>"#.into(),
                    usage_input: 1,
                    usage_output: 1,
                    stop_reason: AgentStopReason::Completed,
                    tool_events: vec![ParserEvent::UseTool {
                        name: "record_analysis".into(),
                        input: json!({"kind": "no_action", "summary": "观望"}),
                    }],
                })
            } else {
                Ok(ProviderTurnOutcome {
                    text: "完成。".into(),
                    usage_input: 1,
                    usage_output: 1,
                    stop_reason: AgentStopReason::Completed,
                    tool_events: vec![],
                })
            }
        }
    }

    fn services() -> (Arc<RuntimeServices>, Arc<AgentRuntimeRepo>) {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        let repo = Arc::new(AgentRuntimeRepo::new(db.clone()));
        let strategy = Arc::new(StrategyService::new(repo.clone()));
        strategy.seed_baseline_if_empty().unwrap();
        // 默认关闭自动分析（spec §5）；测试需开启才会消费 buffer。
        crate::pipeline::agent_runtime::settings::RuntimeSettings::new(repo.clone())
            .set_news_auto_analysis_enabled(true)
            .unwrap();
        let channels = ProviderChannelsRepo::new(db.clone());
        channels
            .add(&ProviderChannel {
                channel_id: "c1".into(),
                provider: "anthropic".into(),
                wire_format: WireFormat::Messages,
                base_url: None,
                api_key: "k".into(),
                model: "m".into(),
                stream: true,
                enabled: true,
                supports_vision: false,
                supports_thinking: false,
                max_output_tokens: None,
                context_window_tokens: None,
                thinking_budget_tokens: None,
            })
            .unwrap();
        channels.set_active("c1").unwrap();
        let messages_repo = AgentMessagesRepo::new(db.clone());
        let deps = RuntimeToolDeps {
            quotes: Arc::new(StubGw),
            news: Arc::new(StubGw),
            account: Arc::new(StubGw),
            strategy: strategy.clone(),
            records: Arc::new(RecordService::new(repo.clone())),
            persist: Some((messages_repo.clone(), PayloadStore::new(db))),

            operate_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        let factory: ProviderFactory =
            Arc::new(|_| Ok(vec![Box::new(FakeProvider { turn: 0 }) as Box<dyn ProviderStream>]));
        let svc = Arc::new(RuntimeServices::new(RuntimeServicesConfig {
            runs: Arc::new(RunService::new(repo.clone())),
            strategy,
            triggers: Arc::new(TriggerRouter::new(repo.clone())),
            news_buffer: Arc::new(NewsBufferService::new(
                repo.clone(),
                NewsBufferConfig {
                    batch_size: 50,
                    max_wait_secs: 600,
                    window_secs: 14400,
                },
            )),
            deps,

            runtime_repo: repo.clone(),
            channels,
            messages_repo,
            provider_factory: factory,
            augment: None,
            event_sink: None,
            max_turns: 2,
            token_budget: None,
            reports_dir: std::env::temp_dir().join("gangzi-test-reviews"),
            review_min_sample_trades: 30,
            eval_batch_size: 200,
            settings: std::sync::Arc::new(
                crate::pipeline::agent_runtime::settings::RuntimeSettings::new(repo.clone()),
            ),
            buffer_dropped_sink: None,
        }));
        (svc, repo)
    }

    #[tokio::test]
    async fn no_pending_does_not_trigger() {
        let (svc, _repo) = services();
        let (triggered, _) = drain_news_buffer(&svc, 0).await;
        assert!(!triggered);
    }

    #[tokio::test]
    async fn below_m_before_n_does_not_trigger() {
        let (svc, _repo) = services();
        svc.news_buffer
            .ingest(&[("n1".into(), None)], Utc::now())
            .unwrap();
        // pending=1 < M=50, elapsed 0 < N=600 → 不触发。
        let (triggered, _) = drain_news_buffer(&svc, 0).await;
        assert!(!triggered);
        assert_eq!(svc.news_buffer.pending_count().unwrap(), 1);
    }

    #[tokio::test]
    async fn n_elapsed_triggers_and_drains() {
        let (svc, repo) = services();
        svc.news_buffer
            .ingest(&[("n1".into(), None), ("n2".into(), None)], Utc::now())
            .unwrap();
        // elapsed 达 N → 触发 drain → 跑一批 → mark_analyzed。
        let (triggered, run_id) = drain_news_buffer(&svc, 600).await;
        assert!(triggered);
        assert!(run_id.is_some());
        // 批已 analyzed（不再 pending）。
        assert_eq!(svc.news_buffer.pending_count().unwrap(), 0);
        // run 已完成落库。
        let run = repo.get_run(&run_id.unwrap()).unwrap().unwrap();
        assert_eq!(
            run.status,
            crate::domain::agent::runtime::AgentRunStatus::Completed
        );
    }
}
