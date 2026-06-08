//! Agent Runtime —— 产品里的 Agent 应用层（编排 + 记账）。
//!
//! Spec: docs/design/agent-runtime-module.md
//!
//! 职责：何时唤起谁、注入哪些工具与策略、风控编排、把结果沉淀成可按 `run_id` 串的决策链。
//! 不拥有交易/新闻/行情底层规则（各 BC）；不跑 LLM loop（Infra）。
//!
//! 模块：
//! - [`strategy`] — InvestmentStrategy 读写 / 版本 / seed baseline（单点写）
//! - [`runs`]     — AgentRun 生命周期 + 策略版本冻结 + run 起止事件
//!
//! 后续（WP1 收尾 / WP2+）：context（L1+L2+L3）/ tools（按 mode 注册）/ records /
//! news_buffer / triggers / review / risk / scheduler。

pub mod bootstrap;
pub mod context;
pub mod executor;
pub mod gateway_impls;
pub mod gateways;
pub mod handlers;
pub mod news_buffer;
pub mod orchestrator;
pub mod records;
pub mod recovery;
pub mod runs;
pub mod scheduler;
pub mod settings;
pub mod strategy;
pub mod tools;
pub mod triggers;
pub mod wiring;

pub use bootstrap::{build_runtime_services, RuntimeBootstrap};
pub use context::{
    build_context, build_intraday_intents_section, IntradayIntentInput, RealtimeSection,
};
pub use gateway_impls::{AccountGatewayImpl, NewsGatewayImpl, QuotesGatewayImpl};
pub use executor::{execute_run, ExecError, ExecuteRunParams};
pub use gateways::{AccountGateway, GatewayError, NewsGateway, QuotesGateway};
pub use handlers::{
    FetchAccountHandler, FetchNewsHandler, FetchQuotesHandler, OperateAccountHandler,
    UpdateWatchlistHandler, UpsertStrategyHandler,
};
pub use news_buffer::{NewsBufferConfig, NewsBufferService};
pub use orchestrator::{
    OrchestrationError, ProviderFactory, RuntimeServices, RuntimeServicesConfig,
};
pub use records::RecordService;
pub use recovery::{RecoveryService, RecoverySummary};
pub use settings::RuntimeSettings;
pub use scheduler::{
    drain_news_buffer, spawn_account_eval_tick_scheduler, spawn_news_buffer_scheduler,
    RuntimeSchedulerHandle,
};
pub use tools::domain_tools_for_mode;
pub use triggers::{Attribution, TriggerRouter};
pub use wiring::{build_domain_registry_for_mode, RuntimeToolDeps};
pub use runs::{RunService, RuntimeEvent, RuntimeEventSink};
pub use strategy::{StrategyError, StrategyService, BASELINE_STRATEGY_ID};
