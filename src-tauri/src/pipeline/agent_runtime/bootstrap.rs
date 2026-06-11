//! Runtime DI 装配 —— 把 BC 真 service + Agent Infra 件组装成 `RuntimeServices`。
//!
//! Spec: docs/design/agent-runtime-module.md §10 实现映射
//!
//! 本函数住在 pipeline 层、**不依赖 tauri**：前端事件桥以闭包（`event_sink` /
//! `runtime_event_sink`）从 adapter/bootstrap 传入，捕获 `AppHandle`。Runtime 只调闭包，不知 tauri。
//!
//! 工具集（spec §3 mode 表）：dialogue/news/account_trigger = 领域工具（fetch_*/operate/
//! update_watchlist/upsert）；review = 只读 fetch_* + write_file（报告）。Infra 工具（本地
//! file/bash + fork run_subagent/run_skill + create_skill）经 `RegistryAugment` 注入 per-run
//! registry（spec §4 / agent-infra §3.6）。

use std::sync::Arc;

use crate::infrastructure::agent::http_provider::HttpProvider;
use crate::infrastructure::agent::loop_executor::ProviderStream;
use crate::infrastructure::agent::messages_repo::AgentMessagesRepo;
use crate::infrastructure::agent::payload_store::PayloadStore;
use crate::infrastructure::agent::channels_repo::ProviderChannelsRepo;
use crate::infrastructure::agent::runtime_repo::AgentRuntimeRepo;
use crate::infrastructure::agent::tool_registry::ToolRegistry;
use crate::infrastructure::db::AppDb;
use crate::pipeline::account::service::AccountService;
use crate::pipeline::news::service::NewsService;
use crate::pipeline::quotes::service::QuotesService;

use crate::domain::agent::runtime::AgentRunMode;
use crate::domain::agent::SideEffect;

use super::executor::{AgentEventSink, RegistryAugment};
use super::gateway_impls::{AccountGatewayImpl, NewsGatewayImpl, QuotesGatewayImpl};
use super::news_buffer::{NewsBufferConfig, NewsBufferService};
use super::orchestrator::{ProviderFactory, RuntimeServices, RuntimeServicesConfig};
use super::records::RecordService;
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
    /// Infra 全局 ToolRegistry（本地 file/bash + fork run_subagent/run_skill + create_skill）。
    /// `build_runtime_services` 据此构造 `RegistryAugment`，把 Infra 工具注入 per-run registry。
    pub infra_registry: Arc<ToolRegistry>,
    /// 复盘报告落盘目录（`<workspace>/reviews`）。
    pub reports_dir: std::path::PathBuf,
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

    let deps = RuntimeToolDeps {
        quotes: Arc::new(QuotesGatewayImpl::new(b.quotes)),
        news: Arc::new(NewsGatewayImpl::new(b.news)),
        account: Arc::new(AccountGatewayImpl::new(b.account)),
        strategy: strategy.clone(),
        records: records.clone(),
        persist: Some((b.agent_messages_repo.clone(), b.payload_store)),
        // 账户作用域 operate 串行锁（spec §4）：单一模拟账户 → 单个进程级全局锁。
        operate_lock: Arc::new(tokio::sync::Mutex::new(())),
    };

    // 生产 provider 工厂：从 active channel 建一个 HttpProvider。
    let provider_factory: ProviderFactory = Arc::new(|ch| {
        let p = HttpProvider::new(ch.clone())?;
        Ok(vec![Box::new(p) as Box<dyn ProviderStream>])
    });

    // Infra 工具（本地 file/bash + fork run_subagent/run_skill + create_skill 等）注入 per-run registry：
    // 从 Infra 全局 registry 复制所有非领域工具到 per-run registry（spec §4 / agent-infra §3.6）。
    // **review run 只读**（spec §3 mode 表：review = fetch_* + record_review_suggestion）：
    // 只复制无副作用的只读 Infra 工具（SideEffect::None 且非 spawn 且非 run_bash——bash 可写盘，
    // 约定级沙箱挡不住），write_file / edit_file / create_skill / run_subagent / run_skill 都不给。
    let infra_reg = b.infra_registry;
    let augment: Option<RegistryAugment> = Some(Arc::new(
        move |per_run: &Arc<ToolRegistry>, mode: AgentRunMode| {
            let review_readonly = mode == AgentRunMode::Review;
            for spec in infra_reg.list_tools() {
                // 领域工具已由 build_domain_registry_for_mode 注册；只复制 Infra 侧工具。
                if per_run.has_tool(&spec.name) {
                    continue; // 已注册（领域工具），跳过
                }
                if review_readonly
                    && (spec.side_effect != SideEffect::None
                        || spec.is_spawn
                        || spec.name == "run_bash")
                {
                    continue;
                }
                if let Some(handler) = infra_reg.clone_handler(&spec.name) {
                    let _ = per_run.register_tool(spec, handler);
                }
            }
        },
    ));

    RuntimeServices::new(RuntimeServicesConfig {
        runs,
        strategy,
        triggers,
        news_buffer,
        deps,
        runtime_repo,
        channels: b.channels,
        messages_repo: b.agent_messages_repo,
        provider_factory,
        augment,
        event_sink: b.agent_event_sink,
        // 以下护栏阈值从 settings 读（spec §8）。
        max_turns: settings.agent_run_max_turns(),
        token_budget: Some(settings.agent_run_token_budget()),
        reports_dir: b.reports_dir,
        review_min_sample_trades: settings.review_min_sample_trades(),
        eval_batch_size: settings.account_trigger_eval_batch_size(),
        settings,
        buffer_dropped_sink: b.buffer_dropped_sink,
    })
}
