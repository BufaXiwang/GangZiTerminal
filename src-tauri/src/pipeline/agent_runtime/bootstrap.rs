//! Runtime DI 装配 —— 把 BC 真 service + Agent Infra 件组装成 `RuntimeServices`。
//!
//! Spec: docs/design/agent-runtime-module.md §10 实现映射
//!
//! 本函数住在 pipeline 层、**不依赖 tauri**：前端事件桥以闭包（`event_sink` /
//! `runtime_event_sink`）从 adapter/bootstrap 传入，捕获 `AppHandle`。Runtime 只调闭包，不知 tauri。
//!
//! 工具集（spec §3 mode 表）：dialogue/news/account_trigger = 领域工具（fetch_*/operate/
//! update_watchlist/upsert）；review = 只读 fetch_* + write_file（报告）。**本地 file/bash 工具不属
//! 交易 agent 工具集**，故 `augment=None`；Infra `run_subagent`（临时复盘 fork）+ review 的
//! `write_file` 待 Infra ForkRuntime 接线后经 augment 注入（见 §WP2 余）。

use std::sync::Arc;

use crate::infrastructure::agent::http_provider::HttpProvider;
use crate::infrastructure::agent::loop_executor::ProviderStream;
use crate::infrastructure::agent::messages_repo::AgentMessagesRepo;
use crate::infrastructure::agent::payload_store::PayloadStore;
use crate::infrastructure::agent::channels_repo::ProviderChannelsRepo;
use crate::infrastructure::agent::runtime_repo::AgentRuntimeRepo;
use crate::infrastructure::db::AppDb;
use crate::pipeline::account::service::AccountService;
use crate::pipeline::news::service::NewsService;
use crate::pipeline::quotes::service::QuotesService;

use super::executor::AgentEventSink;
use super::gateway_impls::{AccountGatewayImpl, NewsGatewayImpl, QuotesGatewayImpl};
use super::news_buffer::{NewsBufferConfig, NewsBufferService};
use super::orchestrator::{ProviderFactory, RuntimeServices, RuntimeServicesConfig};
use super::records::RecordService;
use super::risk::RiskConfig;
use super::runs::{RunService, RuntimeEventSink};
use super::settings::RuntimeSettings;
use super::strategy::StrategyService;
use super::triggers::TriggerRouter;
use super::wiring::RuntimeToolDeps;

/// bootstrap 装配 `RuntimeServices` 所需的真实依赖（BC service Arc + Agent Infra 件 + 事件桥）。
pub struct RuntimeBootstrap {
    pub db: AppDb,
    /// Agent Infra 的消息持久化（对话续接 + tool_call 证据）。
    pub agent_messages_repo: AgentMessagesRepo,
    pub payload_store: PayloadStore,
    pub channels: ProviderChannelsRepo,
    pub quotes: Arc<QuotesService>,
    pub news: Arc<NewsService>,
    pub account: Arc<AccountService>,
    /// AgentEvent（token/tool/run 流）→ 前端 `agent-event`。
    pub agent_event_sink: Option<AgentEventSink>,
    /// RuntimeEvent（run 起止 / AnalysisResult）→ 前端 `agent-run-*` / `agent-analysis-result`。
    pub runtime_event_sink: Option<RuntimeEventSink>,
    /// 复盘报告落盘目录（`<workspace>/reviews`）。
    pub reports_dir: std::path::PathBuf,
    /// 熔断状态变更回调（→ 前端 agent-circuit-breaker）；可 None。
    pub circuit_breaker_sink: Option<Arc<dyn Fn(bool, String) + Send + Sync>>,
    /// news age-out 丢弃计数回调（→ 前端 agent-news-buffer-dropped）；可 None。
    pub buffer_dropped_sink: Option<Arc<dyn Fn(u32, u32) + Send + Sync>>,
}

/// 装配并返回 `RuntimeServices`。同时 seed baseline 策略（若空）。
pub fn build_runtime_services(b: RuntimeBootstrap) -> RuntimeServices {
    let runtime_repo = Arc::new(AgentRuntimeRepo::new(b.db.clone()));

    // Runtime settings facade（spec §8）：所有阈值从此读，缺失用缺省、非法 fail-closed+heartbeat。
    let settings = Arc::new(RuntimeSettings::new(runtime_repo.clone()));

    let strategy = Arc::new(StrategyService::new(runtime_repo.clone()));
    if let Err(e) = strategy.seed_baseline_if_empty() {
        tracing::warn!(target: "runtime.bootstrap", error = %e, "seed baseline strategy failed");
    }

    let runs = Arc::new(RunService::new(runtime_repo.clone()));
    let records = Arc::new(RecordService::new(runtime_repo.clone()));
    if let Some(sink) = b.runtime_event_sink.clone() {
        runs.set_event_sink(sink.clone());
        records.set_event_sink(sink);
    }
    let triggers = Arc::new(TriggerRouter::new(runtime_repo.clone()));
    let news_buffer = Arc::new(NewsBufferService::new(
        runtime_repo.clone(),
        // news buffer 阈值从 settings 读（spec §8）。
        NewsBufferConfig {
            batch_size: settings.news_agent_batch_size(),
            max_wait_secs: settings.news_agent_max_wait_secs(),
            window_secs: settings.news_buffer_window_secs(),
        },
    ));

    // 熔断标志（进程级共享）：deps（handler 读）与 RuntimeServices（set_circuit_breaker 写）同持一份。
    // 初值从持久化 settings 读（重启保持，熔断不自动解除，spec §6/§9）。
    let circuit_breaker = Arc::new(std::sync::atomic::AtomicBool::new(
        settings.circuit_breaker_active(),
    ));
    // 风控阈值从 settings 读（spec §8）。
    let risk = RiskConfig {
        max_consecutive_losses: settings.circuit_breaker_max_consecutive_losses(),
        max_daily_drawdown: settings.circuit_breaker_max_daily_drawdown(),
        chasing_guard_pct: settings.chasing_guard_pct(),
    };

    let deps = RuntimeToolDeps {
        quotes: Arc::new(QuotesGatewayImpl::new(b.quotes)),
        news: Arc::new(NewsGatewayImpl::new(b.news)),
        account: Arc::new(AccountGatewayImpl::new(b.account)),
        strategy: strategy.clone(),
        records: records.clone(),
        persist: Some((b.agent_messages_repo.clone(), b.payload_store)),
        risk,
        circuit_breaker: circuit_breaker.clone(),
        // 账户作用域 operate 串行锁（spec §4）：单一模拟账户 → 单个进程级全局锁。
        operate_lock: Arc::new(tokio::sync::Mutex::new(())),
    };

    // 生产 provider 工厂：从 active channel 建一个 HttpProvider。
    let provider_factory: ProviderFactory = Arc::new(|ch| {
        let p = HttpProvider::new(ch.clone())?;
        Ok(vec![Box::new(p) as Box<dyn ProviderStream>])
    });

    RuntimeServices::new(RuntimeServicesConfig {
        runs,
        strategy,
        triggers,
        news_buffer,
        deps,
        risk,
        runtime_repo,
        channels: b.channels,
        messages_repo: b.agent_messages_repo,
        provider_factory,
        // Infra run_subagent（临时复盘 fork）+ review write_file 待 Infra ForkRuntime 接线后经 augment 注入。
        augment: None,
        event_sink: b.agent_event_sink,
        circuit_breaker,
        // 以下护栏阈值从 settings 读（spec §8）。
        max_turns: settings.agent_run_max_turns(),
        token_budget: Some(settings.agent_run_token_budget()),
        reports_dir: b.reports_dir,
        review_min_sample_trades: settings.review_min_sample_trades(),
        eval_batch_size: settings.account_trigger_eval_batch_size(),
        circuit_breaker_sink: b.circuit_breaker_sink,
        settings,
        buffer_dropped_sink: b.buffer_dropped_sink,
    })
}
