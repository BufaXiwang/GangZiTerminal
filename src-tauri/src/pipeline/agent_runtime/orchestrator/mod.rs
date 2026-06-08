//! Trigger 入口编排 —— 把 4 类 trigger 各跑成一次 AgentRun（WP2 编排层）。
//!
//! Spec: docs/design/agent-runtime-module.md §6 编排流
//!
//! `execute_run`（executor.rs）是脊梁；本层负责**采集**每类 trigger 的 L3 实时上下文、
//! 备好 providers（从 active channel 建）、组装 `ExecuteRunParams` 后调脊梁。
//!
//! - dialogue：用户消息 → 连续对话线程（conversation_id），L3 = 当日已下单意图。
//! - news：buffer 一批 news_ids → 隔离单次，L3 = 本批 news + 当日已下单意图。
//! - account_trigger：账户事件 → 隔离单次，L3 = 当日已下单意图（+ 归因到原始建仓 run）。
//! - review：收盘 / fork → 只读，L3 = scope 决策链 + 基准（review 报告写出待 fork 接线）。
//!
//! provider 构造经 `ProviderFactory` seam：生产用 `HttpProvider`，测试注入 fake。
//!
//! ## 子模块
//!
//! `impl RuntimeServices` 按职责域拆分到以下子模块：
//! - [`dialogue`]             — dialogue trigger（用户消息 → 连续对话）
//! - [`news_orchestration`]   — news batch trigger + buffer drain / age-out / 开关
//! - [`trigger_orchestration`]— account_trigger + eval tick + rescan
//! - [`review_orchestration`] — EOD review + 基准 / follow-up / 报告落盘
//! - [`risk_monitor`]         — 熔断监控 + cancel_run
//! - [`state_query`]          — fetch_agent_state

mod dialogue;
mod news_orchestration;
mod trigger_orchestration;
mod review_orchestration;
mod risk_monitor;
mod state_query;

pub use dialogue::DialogueRunResult;
pub use review_orchestration::ReviewRunResult;
pub use risk_monitor::{CancelRunOutcome, CancelRunStatus};
pub use state_query::{AgentStateSnapshot, StateInclude};

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use chrono::{DateTime, FixedOffset, TimeZone, Utc};

use crate::domain::agent::channel::ProviderChannel;
use crate::domain::agent::messages::{AgentMessage, AgentMessageBlock, AgentMessageRole};
use crate::infrastructure::agent::channels_repo::ProviderChannelsRepo;
use crate::infrastructure::agent::loop_executor::{LoopError, ProviderStream};
use crate::infrastructure::agent::messages_repo::AgentMessagesRepo;
use crate::infrastructure::agent::runtime_repo::AgentRuntimeRepo;

use super::context::{build_intraday_intents_section, IntradayIntentInput, RealtimeSection};
use super::executor::{AgentEventSink, ExecError, RegistryAugment};
use super::news_buffer::NewsBufferService;
use super::risk::RiskConfig;
use super::runs::RunService;
use super::strategy::StrategyService;
use super::triggers::TriggerRouter;
use super::wiring::RuntimeToolDeps;

/// `account_trigger_eval_batch_size` 缺省（spec §8）；真源在 `settings::DEFAULT_ACCOUNT_TRIGGER_EVAL_BATCH_SIZE`。
pub use super::settings::DEFAULT_ACCOUNT_TRIGGER_EVAL_BATCH_SIZE as ACCOUNT_TRIGGER_EVAL_BATCH_SIZE_DEFAULT;
/// 单次行情驱动评估的最大分页 batch 数（防一次刷新内死循环；剩余留兜底定时器/下次刷新）。
const MAX_EVAL_DRAIN_BATCHES: u32 = 50;

/// 从 active channel 构造一次 run 的 provider 链（生产 = HttpProvider；测试注入 fake）。
pub type ProviderFactory =
    Arc<dyn Fn(&ProviderChannel) -> Result<Vec<Box<dyn ProviderStream>>, LoopError> + Send + Sync>;

/// 编排失败。
#[derive(Debug, thiserror::Error)]
pub enum OrchestrationError {
    #[error("无 active provider channel：请先在设置中配置并激活一个模型渠道")]
    NoActiveChannel,
    #[error("channel 读取失败: {0}")]
    Channels(String),
    #[error("provider 构造失败: {0}")]
    Provider(String),
    #[error(transparent)]
    Exec(#[from] ExecError),
    #[error("db error: {0}")]
    Db(#[from] rusqlite::Error),
}

impl OrchestrationError {
    /// news drain 失败分流（spec §5）：可恢复 → 本批回 pending 下窗重试；不可恢复 → dropped。
    ///
    /// 渠道缺失 / provider 构造失败 / run 执行失败 / db 临时错都视为**可恢复**（环境/瞬时问题，
    /// 重试可愈）；目前无产生「不可恢复」的路径（news_ids 是合法字符串，不会因输入永久失败），
    /// 故恒 `true`。保留此分类点以备未来出现确定性致命错误时下沉为 `dropped`。
    pub fn is_recoverable(&self) -> bool {
        true
    }
}

/// 编排服务容器：持有 Runtime 全部服务 + gateway + provider 工厂 + 注入 hook。
///
/// bootstrap 构造一次、`app.manage`；scheduler / Tauri command 调它的 `run_*` 入口。
pub struct RuntimeServices {
    pub runs: Arc<RunService>,
    pub strategy: Arc<StrategyService>,
    pub triggers: Arc<TriggerRouter>,
    pub news_buffer: Arc<NewsBufferService>,
    pub deps: RuntimeToolDeps,
    pub risk: RiskConfig,
    runtime_repo: Arc<AgentRuntimeRepo>,
    channels: ProviderChannelsRepo,
    messages_repo: AgentMessagesRepo,
    provider_factory: ProviderFactory,
    augment: Option<RegistryAugment>,
    event_sink: Option<AgentEventSink>,
    /// 熔断标志（与 deps 内 operate_account handler 同持一份）。
    circuit_breaker: Arc<AtomicBool>,
    /// 在跑 run 的取消令牌表（cancel_run 据 run_id 取消）。
    cancel_registry: Arc<super::executor::CancelRegistry>,
    /// 自动熔断激活时回调（→ 前端 agent-circuit-breaker）；可 None。
    circuit_breaker_sink: Option<Arc<dyn Fn(bool, String) + Send + Sync>>,
    max_turns: u32,
    /// 每 run 输出 token 预算（spec §11 护栏）；None = 不限。
    token_budget: Option<u32>,
    /// 复盘报告落盘目录（`<workspace>/reviews`）。
    reports_dir: std::path::PathBuf,
    /// 样本量护栏：当日有效交易笔数 < 此值 → 报告顶部声明「样本不足」，禁绩效结论（spec §3）。
    review_min_sample_trades: u32,
    /// 行情驱动账户触发评估的分页 batch size（spec §8 `account_trigger_eval_batch_size`，缺省 200）。
    eval_batch_size: u32,
    /// Runtime settings facade（熔断状态持久化：自动 trip / 用户 resume 同步写回，重启保持）。
    settings: Arc<super::settings::RuntimeSettings>,
    /// news batch in-flight lock（spec §8 lock 表 `agent.news_batch`）：同一时刻只有一个 news
    /// batch 在跑。drain 路径 `try_lock` 持有；持锁期跨越「take→mark_in_batch→run→drain」整段。
    news_batch_lock: Arc<tokio::sync::Mutex<()>>,
    /// news age-out 丢弃计数回调（→ 前端 `agent-news-buffer-dropped`，spec §5/§7）；可 None。
    buffer_dropped_sink: Option<Arc<dyn Fn(u32, u32) + Send + Sync>>,
}

/// 构造 `RuntimeServices` 的入参（bootstrap 填）。
pub struct RuntimeServicesConfig {
    pub runs: Arc<RunService>,
    pub strategy: Arc<StrategyService>,
    pub triggers: Arc<TriggerRouter>,
    pub news_buffer: Arc<NewsBufferService>,
    pub deps: RuntimeToolDeps,
    pub risk: RiskConfig,
    pub runtime_repo: Arc<AgentRuntimeRepo>,
    pub channels: ProviderChannelsRepo,
    pub messages_repo: AgentMessagesRepo,
    pub provider_factory: ProviderFactory,
    pub augment: Option<RegistryAugment>,
    pub event_sink: Option<AgentEventSink>,
    pub circuit_breaker: Arc<AtomicBool>,
    pub max_turns: u32,
    pub token_budget: Option<u32>,
    pub reports_dir: std::path::PathBuf,
    pub review_min_sample_trades: u32,
    /// 行情驱动评估的分页 batch size（spec §8 `account_trigger_eval_batch_size`，缺省 200）。
    pub eval_batch_size: u32,
    /// 熔断激活回调（→ 前端 agent-circuit-breaker）；可 None。
    pub circuit_breaker_sink: Option<Arc<dyn Fn(bool, String) + Send + Sync>>,
    /// Runtime settings facade（熔断状态持久化）。
    pub settings: Arc<super::settings::RuntimeSettings>,
    /// news age-out 丢弃计数回调（→ 前端 `agent-news-buffer-dropped`）；可 None。
    pub buffer_dropped_sink: Option<Arc<dyn Fn(u32, u32) + Send + Sync>>,
}

impl RuntimeServices {
    pub fn new(cfg: RuntimeServicesConfig) -> Self {
        Self {
            runs: cfg.runs,
            strategy: cfg.strategy,
            triggers: cfg.triggers,
            news_buffer: cfg.news_buffer,
            deps: cfg.deps,
            risk: cfg.risk,
            runtime_repo: cfg.runtime_repo,
            channels: cfg.channels,
            messages_repo: cfg.messages_repo,
            provider_factory: cfg.provider_factory,
            augment: cfg.augment,
            event_sink: cfg.event_sink,
            circuit_breaker: cfg.circuit_breaker,
            max_turns: cfg.max_turns,
            token_budget: cfg.token_budget,
            reports_dir: cfg.reports_dir,
            review_min_sample_trades: cfg.review_min_sample_trades,
            eval_batch_size: cfg.eval_batch_size,
            cancel_registry: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            circuit_breaker_sink: cfg.circuit_breaker_sink,
            settings: cfg.settings,
            news_batch_lock: Arc::new(tokio::sync::Mutex::new(())),
            buffer_dropped_sink: cfg.buffer_dropped_sink,
        }
    }

    /// 读 active channel；无则报错（自动下单 trigger 在无 channel 时不应起跑）。
    fn active_channel(&self) -> Result<ProviderChannel, OrchestrationError> {
        self.channels
            .active()
            .map_err(|e| OrchestrationError::Channels(e.to_string()))?
            .ok_or(OrchestrationError::NoActiveChannel)
    }

    fn providers(&self, ch: &ProviderChannel) -> Result<Vec<Box<dyn ProviderStream>>, OrchestrationError> {
        (self.provider_factory)(ch).map_err(|e| OrchestrationError::Provider(e.to_string()))
    }

    // ---------------------------------------------------------------- L3 采集

    /// 采集「当日已下单意图」段（防自我打架，spec §6）：今日 AgentTrades + 活跃挂单 + 持仓 + 剩余额度。
    async fn collect_intraday_intents(&self) -> RealtimeSection {
        let day_start = china_day_start(Utc::now());
        let trades = self
            .deps
            .records
            .list_trades_since(day_start)
            .unwrap_or_default();

        // 活跃挂单 + 持仓：读账户（失败不致命 → 退化为占位文本）。
        let (orders_summary, positions_summary) = match self
            .deps
            .account
            .fetch(serde_json::json!({"include": {"orders": true, "positions": true}}))
            .await
        {
            Ok(j) => (summarize_orders(&j), summarize_positions(&j)),
            Err(_) => ("（账户读取失败）".to_string(), "（账户读取失败）".to_string()),
        };

        // 剩余可下单额度（spec §6）：cap = 账户级 `maxDailyNewOrders`（gateway 取，不硬编码）；
        // 已用 = 当日**新开仓**笔数（平仓/调仓不计）。cap=u32::MAX（账户不限）→ 不显示额度。
        let cap = self.deps.account.max_daily_new_orders();
        let remaining_quota = if cap == u32::MAX {
            None
        } else {
            let opens = trades
                .iter()
                .filter(|t| t.account_input_summary.starts_with("[open] "))
                .count() as u32;
            Some(cap.saturating_sub(opens))
        };

        build_intraday_intents_section(IntradayIntentInput {
            trades: &trades,
            active_orders_summary: orders_summary,
            positions_summary,
            remaining_quota,
        })
    }

    // State query / review / risk / trigger / news / dialogue methods are in sub-modules:
    // dialogue.rs, news_orchestration.rs, trigger_orchestration.rs,
    // review_orchestration.rs, risk_monitor.rs, state_query.rs
}

// ============================================================ helpers

fn user_msg_with_id(message_id: &str, text: &str) -> AgentMessage {
    AgentMessage {
        message_id: message_id.to_string(),
        run_id: None,
        conversation_id: None,
        seq: None,
        kind: None,
        role: AgentMessageRole::User,
        blocks: vec![AgentMessageBlock::Text {
            text: text.to_string(),
        }],
        created_at: Utc::now(),
    }
}

/// 自主 run（news / account_trigger / review）的 user-role 任务指令消息（spec §3 L3 / §6）。
///
/// 这三类 run 的 L1/L2/L3 全在 system prompt，`input` 若为空则发给 messages API 的 `messages` 数组为空
/// → 真 Anthropic 格式 provider 直接 400「Messages array cannot be empty」。注入一条任务指令使 messages
/// 非空，并驱动 agent 本轮的工具使用。`message_id` 为生成的临时 id（这类 run 不续接持久对话线程）。
fn autonomous_task_msg(text: &str) -> AgentMessage {
    user_msg_with_id(&format!("task_{}", uuid::Uuid::new_v4()), text)
}

/// 解析 `data:image/png;base64,<base64data>` → `(mime, decoded_bytes)`。格式不对返回 None。
fn parse_data_url_image(data_url: &str) -> Option<(String, Vec<u8>)> {
    let rest = data_url.strip_prefix("data:")?;
    let (header, b64) = rest.split_once(",")?;
    let mime = header.strip_suffix(";base64")?;
    if !mime.starts_with("image/") {
        return None;
    }
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    Some((mime.to_string(), bytes))
}

/// 当日 0 点（Asia/Shanghai，UTC+8）对应的 UTC 瞬间 —— A 股交易日边界。
fn china_day_start(now: DateTime<Utc>) -> DateTime<Utc> {
    let cn = FixedOffset::east_opt(8 * 3600).expect("valid offset");
    let local = now.with_timezone(&cn);
    let start_naive = local
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight valid");
    cn.from_local_datetime(&start_naive)
        .single()
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(now)
}

/// 从 fetch_account JSON 渲染活跃挂单摘要（防御式：字段缺失退化为「无」）。
fn summarize_orders(j: &serde_json::Value) -> String {
    let Some(orders) = j.get("orders").and_then(|v| v.as_array()) else {
        return "无".to_string();
    };
    let lines: Vec<String> = orders
        .iter()
        .filter_map(|o| {
            let code = o.get("tsCode").and_then(|v| v.as_str())?;
            let side = o.get("side").and_then(|v| v.as_str()).unwrap_or("?");
            let status = o.get("status").and_then(|v| v.as_str()).unwrap_or("?");
            Some(format!("{code} {side} [{status}]"))
        })
        .collect();
    if lines.is_empty() {
        "无".to_string()
    } else {
        lines.join("；")
    }
}

/// 从 fetch_account JSON 渲染持仓摘要。
fn summarize_positions(j: &serde_json::Value) -> String {
    let Some(positions) = j.get("positions").and_then(|v| v.as_array()) else {
        return "无".to_string();
    };
    let lines: Vec<String> = positions
        .iter()
        .filter_map(|p| {
            let code = p.get("tsCode").and_then(|v| v.as_str())?;
            let qty = p.get("quantity").and_then(|v| v.as_i64()).unwrap_or(0);
            Some(format!("{code} ×{qty}"))
        })
        .collect();
    if lines.is_empty() {
        "无".to_string()
    } else {
        lines.join("；")
    }
}

/// 从 fetch_news JSON 渲染本批 news 标题/来源清单。
fn summarize_news(j: &serde_json::Value) -> String {
    let Some(items) = j.get("items").and_then(|v| v.as_array()) else {
        return "（本批无 news）".to_string();
    };
    if items.is_empty() {
        return "（本批无 news）".to_string();
    }
    items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            let title = it.get("title").and_then(|v| v.as_str()).unwrap_or("(无标题)");
            let source = it.get("source").and_then(|v| v.as_str()).unwrap_or("?");
            let id = it.get("id").and_then(|v| v.as_str()).unwrap_or("");
            format!("{}) [{source}] {title}（id={id}）", i + 1)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::channel::WireFormat;
    use crate::domain::agent::context::ContextBundle;
    use crate::domain::agent::events::AgentEvent;
    use crate::domain::agent::runtime::{AccountResultRef, AgentRunStatus, AgentRunTrigger};
    use crate::domain::agent::AgentStopReason;
    use crate::domain::shared::TradeDate;
    use crate::infrastructure::agent::loop_executor::ProviderTurnOutcome;
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::agent::payload_store::PayloadStore;
    use crate::infrastructure::agent::runtime_repo::AgentRuntimeRepo;
    use crate::infrastructure::db::{run_migrations, AppDb};
    use crate::pipeline::agent_runtime::gateways::{
        AccountGateway, EvalPage, GatewayError, NewsGateway, OperateOutcome, QuotesGateway,
        UnhandledTrigger,
    };
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
    use crate::pipeline::agent_runtime::news_buffer::NewsBufferConfig;
    use crate::pipeline::agent_runtime::records::RecordService;
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
            // 模拟按 ids 取回两条 news。
            Ok(json!({"items": [
                {"id": "n1", "title": "某公司中标大单", "source": "cls"},
                {"id": "n2", "title": "行业景气度回升", "source": "ths"}
            ]}))
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

    /// 追踪型 QuotesGateway：记录 focused `refresh_quotes` 收到的 codes（验证账户自驱 tick 刷新）。
    struct TrackingQuotesGw {
        core: Vec<String>,
        refreshed: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    }
    #[async_trait::async_trait]
    impl QuotesGateway for TrackingQuotesGw {
        async fn fetch(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
        fn core_indexes(&self) -> Vec<String> {
            self.core.clone()
        }
        async fn refresh_quotes(&self, ts_codes: Vec<String>) -> Result<(), GatewayError> {
            self.refreshed.lock().unwrap().push(ts_codes);
            Ok(())
        }
    }

    /// 计数型 AccountGateway：记录 rebuild / eval 调用次数，eval 按预设分页返回（消费完游标后 has_more=false）。
    struct CountingAccountGw {
        rebuilds: Arc<AtomicU32>,
        evals: Arc<AtomicU32>,
        /// 模拟分页：第 N 批返回 has_more（N 从 0 起），直到 `pages` 批耗尽。
        pages: u32,
    }
    impl CountingAccountGw {
        fn new(pages: u32) -> (Arc<Self>, Arc<AtomicU32>, Arc<AtomicU32>) {
            let rebuilds = Arc::new(AtomicU32::new(0));
            let evals = Arc::new(AtomicU32::new(0));
            (
                Arc::new(Self {
                    rebuilds: rebuilds.clone(),
                    evals: evals.clone(),
                    pages,
                }),
                rebuilds,
                evals,
            )
        }
    }
    #[async_trait::async_trait]
    impl AccountGateway for CountingAccountGw {
        async fn fetch(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
        async fn operate(&self, _: JsonValue, _: &str, _: &str) -> OperateOutcome {
            OperateOutcome::from_result(AccountResultRef::default())
        }
        async fn update_watchlist(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
        // 非空关注集合 → 账户自驱 tick 不跳过。
        fn subscribed_codes(&self) -> Vec<String> {
            vec!["600519.SH".to_string()]
        }
        async fn rebuild_account_snapshot(&self) -> Result<(), GatewayError> {
            self.rebuilds.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
        async fn evaluate_account_triggers(
            &self,
            _cursor: Option<String>,
            _batch_size: u32,
        ) -> Result<EvalPage, GatewayError> {
            // 第 n 次调用（0-based）。n < pages-1 时 has_more=true 给下一页游标。
            let n = self.evals.fetch_add(1, AtomicOrdering::SeqCst);
            let has_more = n + 1 < self.pages;
            Ok(EvalPage {
                has_more,
                next_cursor: if has_more {
                    Some(format!("cursor-{}", n + 1))
                } else {
                    None
                },
            })
        }
    }

    /// 同 CountingAccountGw 但 `subscribed_codes` 为空（空仓无挂单无自选）——验证仅 core_indexes 也驱动 tick。
    struct CountingAccountGwNoSubs {
        rebuilds: Arc<AtomicU32>,
        evals: Arc<AtomicU32>,
        pages: u32,
    }
    impl CountingAccountGwNoSubs {
        fn new(pages: u32) -> (Arc<Self>, Arc<AtomicU32>, Arc<AtomicU32>) {
            let rebuilds = Arc::new(AtomicU32::new(0));
            let evals = Arc::new(AtomicU32::new(0));
            (
                Arc::new(Self {
                    rebuilds: rebuilds.clone(),
                    evals: evals.clone(),
                    pages,
                }),
                rebuilds,
                evals,
            )
        }
    }
    #[async_trait::async_trait]
    impl AccountGateway for CountingAccountGwNoSubs {
        async fn fetch(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
        async fn operate(&self, _: JsonValue, _: &str, _: &str) -> OperateOutcome {
            OperateOutcome::from_result(AccountResultRef::default())
        }
        async fn update_watchlist(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
        // subscribed_codes 默认空（不 override）。
        async fn rebuild_account_snapshot(&self) -> Result<(), GatewayError> {
            self.rebuilds.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
        async fn evaluate_account_triggers(
            &self,
            _cursor: Option<String>,
            _batch_size: u32,
        ) -> Result<EvalPage, GatewayError> {
            let n = self.evals.fetch_add(1, AtomicOrdering::SeqCst);
            let has_more = n + 1 < self.pages;
            Ok(EvalPage {
                has_more,
                next_cursor: if has_more {
                    Some(format!("cursor-{}", n + 1))
                } else {
                    None
                },
            })
        }
    }

    /// 启动恢复 ⑤ 用 mock：list_unhandled_triggers 返回两个未 handled trigger；记录 mark_handled 调用。
    struct UnhandledTriggerGw;
    #[async_trait::async_trait]
    impl AccountGateway for UnhandledTriggerGw {
        async fn fetch(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({"orders": [], "positions": []}))
        }
        async fn operate(&self, _: JsonValue, _: &str, _: &str) -> OperateOutcome {
            OperateOutcome::from_result(AccountResultRef::default())
        }
        async fn update_watchlist(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
        async fn mark_trigger_handled(&self, _trigger_id: &str) -> Result<bool, GatewayError> {
            Ok(true)
        }
        async fn list_unhandled_triggers(
            &self,
            _limit: u32,
        ) -> Result<Vec<UnhandledTrigger>, GatewayError> {
            Ok(vec![
                UnhandledTrigger { trigger_id: "utg1".into(), order_id: None, summary: "止损 A".into() },
                UnhandledTrigger { trigger_id: "utg2".into(), order_id: Some("ordZ".into()), summary: "成交 B".into() },
            ])
        }
    }

    /// 把注入的 ContextBundle 全部 system_parts 文本拼进 sink（供断言注入了哪些上下文），单轮即完成。
    struct EchoContextProvider {
        sink: Arc<std::sync::Mutex<String>>,
    }
    #[async_trait::async_trait]
    impl ProviderStream for EchoContextProvider {
        async fn next_turn(
            &mut self,
            _messages: &[AgentMessage],
            context: &ContextBundle,
            _event_tx: &Sender<AgentEvent>,
            _run_id: &str,
        ) -> Result<ProviderTurnOutcome, LoopError> {
            if let Ok(mut g) = self.sink.lock() {
                for p in &context.system_parts {
                    match &p.content {
                        crate::domain::agent::context::ContextContent::Text(s) => g.push_str(s),
                        crate::domain::agent::context::ContextContent::Json(v) => {
                            g.push_str(&v.to_string())
                        }
                    }
                    g.push('\n');
                }
            }
            Ok(ProviderTurnOutcome {
                text: "复盘完成。".into(),
                usage_input: 1,
                usage_output: 1,
                stop_reason: AgentStopReason::Completed,
                tool_events: vec![],
            })
        }
    }

    /// 捕获 provider 收到的 `messages` 数组长度（回归 Bug：自主 run 发空 messages → provider 400）。
    /// 把首轮收到的 messages 数量记进 sink，单轮即完成。
    struct CaptureMessagesProvider {
        sink: Arc<std::sync::Mutex<usize>>,
    }
    #[async_trait::async_trait]
    impl ProviderStream for CaptureMessagesProvider {
        async fn next_turn(
            &mut self,
            messages: &[AgentMessage],
            _context: &ContextBundle,
            _event_tx: &Sender<AgentEvent>,
            _run_id: &str,
        ) -> Result<ProviderTurnOutcome, LoopError> {
            if let Ok(mut g) = self.sink.lock() {
                *g = messages.len();
            }
            Ok(ProviderTurnOutcome {
                text: "ok".into(),
                usage_input: 1,
                usage_output: 1,
                stop_reason: AgentStopReason::Completed,
                tool_events: vec![],
            })
        }
    }

    struct FakeProvider {
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
                return Err(LoopError::Provider("exhausted".into()));
            }
            self.used = true;
            Ok(ProviderTurnOutcome {
                text: "已分析，no_action。".into(),
                usage_input: 5,
                usage_output: 7,
                stop_reason: AgentStopReason::Completed,
                tool_events: vec![],
            })
        }
    }

    fn active_channel(repo: &ProviderChannelsRepo) {
        let ch = ProviderChannel {
            channel_id: "c1".into(),
            provider: "anthropic".into(),
            wire_format: WireFormat::Messages,
            base_url: None,
            api_key: "k".into(),
            model: "claude".into(),
            stream: true,
            enabled: true,
            supports_vision: false,
            supports_thinking: false,
            max_output_tokens: None,
            context_window_tokens: None,
            thinking_budget_tokens: None,
        };
        repo.add(&ch).unwrap();
        repo.set_active("c1").unwrap();
    }

    fn services(with_channel: bool) -> (RuntimeServices, Arc<AgentRuntimeRepo>) {
        services_full(with_channel, Arc::new(StubGw), Arc::new(StubGw))
    }

    fn services_with_account(
        with_channel: bool,
        account: Arc<dyn AccountGateway>,
    ) -> (RuntimeServices, Arc<AgentRuntimeRepo>) {
        services_full(with_channel, account, Arc::new(StubGw))
    }

    fn services_with_news(
        with_channel: bool,
        news: Arc<dyn NewsGateway>,
    ) -> (RuntimeServices, Arc<AgentRuntimeRepo>) {
        services_full(with_channel, Arc::new(StubGw), news)
    }

    fn services_full(
        with_channel: bool,
        account: Arc<dyn AccountGateway>,
        news: Arc<dyn NewsGateway>,
    ) -> (RuntimeServices, Arc<AgentRuntimeRepo>) {
        services_full_with_quotes(with_channel, account, news, Arc::new(StubGw))
    }

    fn services_full_with_quotes(
        with_channel: bool,
        account: Arc<dyn AccountGateway>,
        news: Arc<dyn NewsGateway>,
        quotes: Arc<dyn QuotesGateway>,
    ) -> (RuntimeServices, Arc<AgentRuntimeRepo>) {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        let repo = Arc::new(AgentRuntimeRepo::new(db.clone()));
        let strategy = Arc::new(StrategyService::new(repo.clone()));
        strategy.seed_baseline_if_empty().unwrap();
        let channels = ProviderChannelsRepo::new(db.clone());
        if with_channel {
            active_channel(&channels);
        }
        let messages_repo = AgentMessagesRepo::new(db.clone());
        let deps = RuntimeToolDeps {
            quotes,
            news,
            account,
            strategy: strategy.clone(),
            records: Arc::new(RecordService::new(repo.clone())),
            persist: Some((messages_repo.clone(), PayloadStore::new(db))),
            risk: RiskConfig::default(),
            circuit_breaker: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            operate_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        let factory: ProviderFactory =
            Arc::new(|_ch| Ok(vec![Box::new(FakeProvider { used: false }) as Box<dyn ProviderStream>]));
        let svc = RuntimeServices::new(RuntimeServicesConfig {
            runs: Arc::new(RunService::new(repo.clone())),
            strategy,
            triggers: Arc::new(TriggerRouter::new(repo.clone())),
            news_buffer: Arc::new(NewsBufferService::new(repo.clone(), NewsBufferConfig::default())),
            deps,
            risk: RiskConfig::default(),
            runtime_repo: repo.clone(),
            channels,
            messages_repo,
            provider_factory: factory,
            augment: None,
            event_sink: None,
            circuit_breaker: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_turns: 3,
            token_budget: None,
            reports_dir: std::env::temp_dir().join("gangzi-test-reviews"),
            review_min_sample_trades: 30,
            eval_batch_size: 200,
        circuit_breaker_sink: None,
            settings: Arc::new(crate::pipeline::agent_runtime::settings::RuntimeSettings::new(repo.clone())),
            buffer_dropped_sink: None,
        });
        (svc, repo)
    }

    /// QuotesGateway mock：`core_indexes()` 返回预设指数；`fetch` 按 tsCodes 回 changePercent。
    struct BenchmarkQuotesGw {
        indexes: Vec<String>,
    }
    #[async_trait::async_trait]
    impl QuotesGateway for BenchmarkQuotesGw {
        async fn fetch(&self, input: JsonValue) -> Result<JsonValue, GatewayError> {
            let codes: Vec<String> = input
                .get("tsCodes")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|c| c.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let items: Vec<JsonValue> = codes
                .iter()
                .enumerate()
                .map(|(i, c)| json!({"tsCode": c, "quote": {"changePercent": 1.0 + i as f64}}))
                .collect();
            Ok(json!({"items": items}))
        }
        fn core_indexes(&self) -> Vec<String> {
            self.indexes.clone()
        }
    }

    /// AccountGateway mock：`daily_return` facade 回固定组合当日收益率（小数）——下沉后 Runtime
    /// 复盘的组合收益率来自 Account facade，不再自扒 snapshot 算（spec account §2 / runtime §3②）。
    struct EquityAccountGw {
        /// 组合当日收益率（小数，如 0.02 = +2.00%）。
        daily_return: f64,
    }
    #[async_trait::async_trait]
    impl AccountGateway for EquityAccountGw {
        async fn fetch(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({"snapshot": {}, "orders": [], "positions": []}))
        }
        async fn operate(&self, _: JsonValue, _: &str, _: &str) -> OperateOutcome {
            OperateOutcome::from_result(AccountResultRef::default())
        }
        async fn update_watchlist(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
        fn daily_return(&self, _now: chrono::DateTime<Utc>) -> Option<f64> {
            Some(self.daily_return)
        }
    }

    /// AccountGateway mock：`consecutive_losses` / `daily_drawdown` facade 回固定值——验证
    /// Runtime monitor_risk 只「调 facade + 比阈值」，不自扒快照（spec runtime §6 熔断）。
    struct RiskAccountGw {
        losses: u32,
        drawdown: f64,
    }
    #[async_trait::async_trait]
    impl AccountGateway for RiskAccountGw {
        async fn fetch(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({"orders": [], "positions": []}))
        }
        async fn operate(&self, _: JsonValue, _: &str, _: &str) -> OperateOutcome {
            OperateOutcome::from_result(AccountResultRef::default())
        }
        async fn update_watchlist(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
        fn consecutive_losses(&self, _now: chrono::DateTime<Utc>) -> u32 {
            self.losses
        }
        fn daily_drawdown(&self, _now: chrono::DateTime<Utc>) -> f64 {
            self.drawdown
        }
    }

    #[tokio::test]
    async fn monitor_risk_trips_breaker_via_account_facade_losses() {
        // 连亏达阈值（默认 5）→ Account facade 回 5 → Runtime 熔断激活。
        let dir = unique_reviews_dir("cb_losses");
        let account = Arc::new(RiskAccountGw { losses: 5, drawdown: 0.0 });
        let (svc, _repo) = review_services_full(dir.clone(), Arc::new(StubGw), account, 30);
        assert!(!svc.circuit_breaker_active());
        let r = svc.monitor_risk().await;
        assert!(r.is_some(), "连亏达阈值应触发熔断");
        assert!(svc.circuit_breaker_active(), "熔断应激活");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn monitor_risk_trips_breaker_via_account_facade_drawdown() {
        // 回撤超阈值（默认 5%）→ Account facade 回 0.06 → Runtime 熔断激活。
        let dir = unique_reviews_dir("cb_dd");
        let account = Arc::new(RiskAccountGw { losses: 0, drawdown: 0.06 });
        let (svc, _repo) = review_services_full(dir.clone(), Arc::new(StubGw), account, 30);
        let r = svc.monitor_risk().await;
        assert!(r.is_some(), "回撤超阈值应触发熔断");
        assert!(svc.circuit_breaker_active());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn monitor_risk_no_trip_below_thresholds() {
        let dir = unique_reviews_dir("cb_none");
        let account = Arc::new(RiskAccountGw { losses: 4, drawdown: 0.04 });
        let (svc, _repo) = review_services_full(dir.clone(), Arc::new(StubGw), account, 30);
        assert!(svc.monitor_risk().await.is_none(), "未达阈值不熔断");
        assert!(!svc.circuit_breaker_active());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// 复盘专用 service 构造（可注入 account gateway，验证组合收益率确定性算）。
    #[allow(clippy::type_complexity)]
    fn review_services_full(
        reports_dir: std::path::PathBuf,
        quotes: Arc<dyn QuotesGateway>,
        account: Arc<dyn AccountGateway>,
        min_sample: u32,
    ) -> (RuntimeServices, Arc<AgentRuntimeRepo>) {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        let repo = Arc::new(AgentRuntimeRepo::new(db.clone()));
        let strategy = Arc::new(StrategyService::new(repo.clone()));
        strategy.seed_baseline_if_empty().unwrap();
        let channels = ProviderChannelsRepo::new(db.clone());
        active_channel(&channels);
        let messages_repo = AgentMessagesRepo::new(db.clone());
        let deps = RuntimeToolDeps {
            quotes,
            news: Arc::new(StubGw),
            account,
            strategy: strategy.clone(),
            records: Arc::new(RecordService::new(repo.clone())),
            persist: Some((messages_repo.clone(), PayloadStore::new(db))),
            risk: RiskConfig::default(),
            circuit_breaker: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            operate_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        let factory: ProviderFactory =
            Arc::new(|_ch| Ok(vec![Box::new(FakeProvider { used: false }) as Box<dyn ProviderStream>]));
        let svc = RuntimeServices::new(RuntimeServicesConfig {
            runs: Arc::new(RunService::new(repo.clone())),
            strategy,
            triggers: Arc::new(TriggerRouter::new(repo.clone())),
            news_buffer: Arc::new(NewsBufferService::new(repo.clone(), NewsBufferConfig::default())),
            deps,
            risk: RiskConfig::default(),
            runtime_repo: repo.clone(),
            channels,
            messages_repo,
            provider_factory: factory,
            augment: None,
            event_sink: None,
            circuit_breaker: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_turns: 3,
            token_budget: None,
            reports_dir,
            review_min_sample_trades: min_sample,
            eval_batch_size: 200,
            circuit_breaker_sink: None,
            settings: Arc::new(crate::pipeline::agent_runtime::settings::RuntimeSettings::new(repo.clone())),
            buffer_dropped_sink: None,
        });
        (svc, repo)
    }

    /// 复盘专用 service 构造：自定义 reports_dir + quotes gateway + min_sample（隔离每个测试的目录，
    /// 避免共享 `gangzi-test-reviews` 目录互相污染文件名/覆盖断言）。
    fn review_services(
        reports_dir: std::path::PathBuf,
        quotes: Arc<dyn QuotesGateway>,
        min_sample: u32,
    ) -> (RuntimeServices, Arc<AgentRuntimeRepo>) {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        let repo = Arc::new(AgentRuntimeRepo::new(db.clone()));
        let strategy = Arc::new(StrategyService::new(repo.clone()));
        strategy.seed_baseline_if_empty().unwrap();
        let channels = ProviderChannelsRepo::new(db.clone());
        active_channel(&channels);
        let messages_repo = AgentMessagesRepo::new(db.clone());
        let deps = RuntimeToolDeps {
            quotes,
            news: Arc::new(StubGw),
            account: Arc::new(StubGw),
            strategy: strategy.clone(),
            records: Arc::new(RecordService::new(repo.clone())),
            persist: Some((messages_repo.clone(), PayloadStore::new(db))),
            risk: RiskConfig::default(),
            circuit_breaker: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            operate_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        let factory: ProviderFactory =
            Arc::new(|_ch| Ok(vec![Box::new(FakeProvider { used: false }) as Box<dyn ProviderStream>]));
        let svc = RuntimeServices::new(RuntimeServicesConfig {
            runs: Arc::new(RunService::new(repo.clone())),
            strategy,
            triggers: Arc::new(TriggerRouter::new(repo.clone())),
            news_buffer: Arc::new(NewsBufferService::new(repo.clone(), NewsBufferConfig::default())),
            deps,
            risk: RiskConfig::default(),
            runtime_repo: repo.clone(),
            channels,
            messages_repo,
            provider_factory: factory,
            augment: None,
            event_sink: None,
            circuit_breaker: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_turns: 3,
            token_budget: None,
            reports_dir,
            review_min_sample_trades: min_sample,
            eval_batch_size: 200,
            circuit_breaker_sink: None,
            settings: Arc::new(crate::pipeline::agent_runtime::settings::RuntimeSettings::new(repo.clone())),
            buffer_dropped_sink: None,
        });
        (svc, repo)
    }

    fn unique_reviews_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("gangzi-rev-{tag}-{nanos}/reviews"))
    }

    #[tokio::test]
    async fn review_report_filename_is_trade_date_and_overwrites() {
        let dir = unique_reviews_dir("fname");
        let (svc, _repo) =
            review_services(dir.clone(), Arc::new(StubGw), 30);
        let date = TradeDate::parse("20260605").unwrap();

        let r1 = svc.run_eod_review(date).await.unwrap();
        let p1 = r1.report_path.expect("应写出报告");
        // 文件名 = <tradeDate>.md（不含 run_id）。
        assert!(p1.ends_with("20260605.md"), "文件名应为 tradeDate.md, got {p1}");

        // 同交易日重跑 → 覆盖、不堆积（目录仍只 1 个 .md，且无 .tmp 残片）。
        let r2 = svc.run_eod_review(date).await.unwrap();
        let p2 = r2.report_path.expect("重跑应写出报告");
        assert_eq!(p1, p2, "同日重跑应覆盖同一文件");
        let mds: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(mds.iter().filter(|n| n.ends_with(".md")).count(), 1, "同日不堆积");
        assert!(!mds.iter().any(|n| n.ends_with(".tmp")), "不留 .tmp 残片: {mds:?}");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn review_report_atomic_write_complete() {
        let dir = unique_reviews_dir("atomic");
        let (svc, _repo) = review_services(dir.clone(), Arc::new(StubGw), 30);
        let date = TradeDate::parse("20260605").unwrap();
        let r = svc.run_eod_review(date).await.unwrap();
        let path = r.report_path.expect("应写出报告");
        let body = std::fs::read_to_string(&path).expect("报告文件应存在且完整");
        // 完整：含 header + 样本声明段 + 基准段 + agent 结论段。
        assert!(body.contains("# 收盘复盘 20260605"));
        assert!(body.contains("## 样本量与置信度声明"));
        assert!(body.contains("## 组合收益 vs 基准"));
        assert!(body.contains("## 上次建议 follow-up"));
        assert!(body.contains("## Agent 复盘结论"));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn review_benchmark_uses_core_indexes() {
        let dir = unique_reviews_dir("bench");
        let quotes = Arc::new(BenchmarkQuotesGw {
            indexes: vec!["000016.SH".into(), "399006.SZ".into()],
        });
        let (svc, _repo) = review_services(dir.clone(), quotes, 30);
        let date = TradeDate::parse("20260605").unwrap();
        let r = svc.run_eod_review(date).await.unwrap();
        let body = std::fs::read_to_string(r.report_path.unwrap()).unwrap();
        // 报告基准段用 core_indexes() 返回的指数（非硬编码 000300/000905）。
        assert!(body.contains("000016.SH"), "基准应含 core_indexes 指数, body=\n{body}");
        assert!(body.contains("399006.SZ"));
        assert!(!body.contains("000300.SH"), "不应硬编码沪深300");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn review_low_sample_declares_insufficient() {
        let dir = unique_reviews_dir("sample");
        // min_sample=30，当日 0 笔交易 → 必然样本不足。
        let (svc, _repo) = review_services(dir.clone(), Arc::new(StubGw), 30);
        let date = TradeDate::parse("20260605").unwrap();
        let r = svc.run_eod_review(date).await.unwrap();
        let body = std::fs::read_to_string(r.report_path.unwrap()).unwrap();
        assert!(
            body.contains("样本不足"),
            "样本不足时报告顶部应有确定性声明, body=\n{body}"
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn review_prev_report_injected_as_followup_context() {
        let dir = unique_reviews_dir("prev");
        std::fs::create_dir_all(&dir).unwrap();
        // 预置上一交易日报告（20260604.md），含可识别标记。
        let marker = "PREV_REVIEW_MARKER_上次建议加仓白酒";
        std::fs::write(dir.join("20260604.md"), format!("# 收盘复盘 20260604\n\n{marker}\n")).unwrap();

        // capture provider：把注入的实时上下文（含上版报告）原样回显，便于断言进了上下文。
        let seen: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new(String::new()));
        let (mut svc, _repo) = review_services(dir.clone(), Arc::new(StubGw), 30);
        // 用一个回显 provider 替换 factory。
        let seen_cap = seen.clone();
        svc.provider_factory = Arc::new(move |_ch| {
            Ok(vec![Box::new(EchoContextProvider { sink: seen_cap.clone() }) as Box<dyn ProviderStream>])
        });

        let date = TradeDate::parse("20260605").unwrap();
        let _ = svc.run_eod_review(date).await.unwrap();
        let captured = seen.lock().unwrap().clone();
        assert!(
            captured.contains(marker),
            "上一交易日报告内容应作为 follow-up 上下文注入 review agent, captured=\n{captured}"
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn review_portfolio_return_and_excess_computed_by_runtime() {
        // 组合当日收益率来自 Account facade `daily_return`（下沉后单一所有者）；超额 = 组合 − 基准
        // 仍由 Runtime 编排相减（跨 BC，spec runtime §3②）。
        // daily_return = 0.02 → 组合 +2.00%。
        // core_indexes: 000016.SH 当日 +1.00% → 超额 = +2.00% − 1.00% = +1.00%；
        //               399006.SZ 当日 +2.00% → 超额 = +2.00% − 2.00% = +0.00%。
        let dir = unique_reviews_dir("excess");
        let quotes = Arc::new(BenchmarkQuotesGw {
            indexes: vec!["000016.SH".into(), "399006.SZ".into()],
        });
        let account = Arc::new(EquityAccountGw { daily_return: 0.02 });
        let (svc, _repo) = review_services_full(dir.clone(), quotes, account, 30);
        let date = TradeDate::parse("20260605").unwrap();

        let r = svc.run_eod_review(date).await.unwrap();
        let body = std::fs::read_to_string(r.report_path.unwrap()).unwrap();
        // 组合当日收益率确定性写出。
        assert!(body.contains("组合当日收益率 +2.00%"), "组合收益率应由 Runtime 算出, body=\n{body}");
        // 逐指数超额（组合 − 基准）确定性写出。
        assert!(body.contains("000016.SH 当日 +1.00%；超额 = 组合 − 基准 = +1.00%"), "body=\n{body}");
        assert!(body.contains("399006.SZ 当日 +2.00%；超额 = 组合 − 基准 = +0.00%"), "body=\n{body}");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn review_followup_reflects_suggestion_adoption() {
        // Gap B：上一交易日登记 ReviewSuggestion → 之后 upsert 策略 → 本次复盘 follow-up 确定性写出
        // 「已采纳 + 活跃版本」。
        let dir = unique_reviews_dir("followup");
        let (svc, _repo) = review_services_full(dir.clone(), Arc::new(StubGw), Arc::new(StubGw), 30);
        let records = svc.deps.records.clone();
        let prev = TradeDate::parse("20260604").unwrap();
        let today = TradeDate::parse("20260605").unwrap();

        // 上一交易日登记一条建议。
        let sugg = records
            .record_review_suggestion("review_run_prev", prev.clone(), "单票上限收紧到 15%")
            .unwrap();
        assert!(sugg.suggestion_id.starts_with("rs_"));
        // 建议读得回。
        assert_eq!(records.list_review_suggestions_by_date(&prev).unwrap().len(), 1);

        // 建议之后用户确认 upsert（baseline v1 → v2）。
        let (_id, v) = svc
            .strategy
            .upsert(None, Some(1), "更保守：单票上限 15%。".into(), crate::domain::agent::runtime::StrategyStatus::Active, "采纳复盘建议")
            .unwrap();
        assert_eq!(v, 2);

        let r = svc.run_eod_review(today).await.unwrap();
        let body = std::fs::read_to_string(r.report_path.unwrap()).unwrap();
        assert!(body.contains("单票上限收紧到 15%"), "follow-up 应含上版建议原文, body=\n{body}");
        assert!(body.contains("已采纳"), "建议后发生过 upsert → 应判定已采纳, body=\n{body}");
        assert!(body.contains("当前活跃版本 v2"), "应写出采纳后活跃版本, body=\n{body}");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn review_followup_marks_not_adopted_when_no_upsert() {
        // Gap B 反面：建议后没有 upsert → follow-up 确定性判定「未采纳」。
        let dir = unique_reviews_dir("notadopt");
        let (svc, _repo) = review_services_full(dir.clone(), Arc::new(StubGw), Arc::new(StubGw), 30);
        let records = svc.deps.records.clone();
        let prev = TradeDate::parse("20260604").unwrap();
        let today = TradeDate::parse("20260605").unwrap();
        records
            .record_review_suggestion("review_run_prev", prev, "建议加仓周期股")
            .unwrap();
        // 不做 upsert。
        let r = svc.run_eod_review(today).await.unwrap();
        let body = std::fs::read_to_string(r.report_path.unwrap()).unwrap();
        assert!(body.contains("建议加仓周期股"), "body=\n{body}");
        assert!(body.contains("未采纳"), "无后续 upsert → 应判定未采纳, body=\n{body}");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn eod_review_writes_report_file() {
        let (svc, repo) = services(true);
        let date = TradeDate::parse("20260605").unwrap();
        let result = svc.run_eod_review(date).await.unwrap();
        assert_eq!(
            repo.get_run(&result.run.run_id).unwrap().unwrap().status,
            AgentRunStatus::Completed
        );
        let path = result.report_path.expect("应写出报告文件");
        let body = std::fs::read_to_string(&path).expect("报告文件应存在");
        assert!(body.contains("# 收盘复盘"), "报告应含标题");
        assert!(body.contains(&result.run.run_id), "报告应含 run_id");
        assert!(svc.list_review_reports().iter().any(|(_, p)| p == &path));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn news_batch_runs_to_completion() {
        let (svc, repo) = services(true);
        let run = svc.run_news_batch(vec!["n1".into(), "n2".into()]).await.unwrap();
        assert_eq!(run.strategy_version, Some(1)); // 冻结 baseline v1
        let stored = repo.get_run(&run.run_id).unwrap().unwrap();
        assert_eq!(stored.status, AgentRunStatus::Completed);
        assert!(repo.list_running_runs().unwrap().is_empty());
    }

    /// 回归（Bug P0）：三类自主 run（news / account_trigger / review）必须给 provider 发**非空** messages
    /// 数组——L1/L2/L3 全在 system prompt，若 `input` 为空则 messages 数组为空 → 真 Anthropic 格式
    /// provider 直接 400「Messages array cannot be empty」。这里用捕获 provider 断言每类 run 收到的
    /// messages ≥ 1。dialogue 本就带用户消息，一并验证。
    #[tokio::test]
    async fn autonomous_runs_send_non_empty_messages() {
        // 每类 run 单独建 services + 捕获 provider（factory 注入捕获 provider）。
        async fn captured_len_for<F, Fut>(run: F) -> usize
        where
            F: FnOnce(RuntimeServices) -> Fut,
            Fut: std::future::Future<Output = ()>,
        {
            let sink: Arc<std::sync::Mutex<usize>> = Arc::new(std::sync::Mutex::new(0));
            let (mut svc, _repo) = services(true);
            let cap = sink.clone();
            svc.provider_factory = Arc::new(move |_ch| {
                Ok(vec![
                    Box::new(CaptureMessagesProvider { sink: cap.clone() }) as Box<dyn ProviderStream>,
                ])
            });
            run(svc).await;
            let n = *sink.lock().unwrap();
            n
        }

        // news
        let n = captured_len_for(|svc| async move {
            let _ = svc.run_news_batch(vec!["n1".into()]).await.unwrap();
        })
        .await;
        assert!(n >= 1, "news run 应发非空 messages，got {n}");

        // account_trigger
        let n = captured_len_for(|svc| async move {
            let _ = svc
                .run_account_trigger("tg1".into(), None, "止损命中 600519".into())
                .await
                .unwrap();
        })
        .await;
        assert!(n >= 1, "account_trigger run 应发非空 messages，got {n}");

        // review
        let n = captured_len_for(|svc| async move {
            let date = TradeDate::parse("20260605").unwrap();
            let _ = svc.run_eod_review(date).await.unwrap();
        })
        .await;
        assert!(n >= 1, "review run 应发非空 messages，got {n}");

        // dialogue（本就带用户消息，回归对照）
        let n = captured_len_for(|svc| async move {
            let _ = svc.run_dialogue("conv1".into(), "你好".into()).await.unwrap();
        })
        .await;
        assert!(n >= 1, "dialogue run 应发非空 messages，got {n}");
    }

    #[tokio::test]
    async fn drain_marks_in_batch_with_run_id_then_analyzed() {
        let (svc, repo) = services(true);
        svc.settings.set_news_auto_analysis_enabled(true).unwrap();
        svc.news_buffer.ingest(&[("n1".into(), None)], Utc::now()).unwrap();
        let run_id = svc.drain_news_batch().await.expect("drain ran a batch");
        // 成功后 status=analyzed 且 run_id 仍是本批 mark_in_batch 标上的 runId（spec §5）。
        let (status, rid) = repo.news_buffer_status("n1").unwrap().unwrap();
        assert_eq!(status, "analyzed");
        assert_eq!(rid.as_deref(), Some(run_id.as_str()));
    }

    #[tokio::test]
    async fn drain_recoverable_failure_reverts_to_pending() {
        // 无 channel → run_news_batch 报 NoActiveChannel（可恢复）→ 本批回 pending（spec §5）。
        let (svc, repo) = services(false);
        svc.settings.set_news_auto_analysis_enabled(true).unwrap();
        svc.news_buffer.ingest(&[("n1".into(), None)], Utc::now()).unwrap();
        let r = svc.drain_news_batch().await;
        assert!(r.is_none()); // 未成功
        let (status, rid) = repo.news_buffer_status("n1").unwrap().unwrap();
        assert_eq!(status, "pending"); // 回 pending 重试
        assert!(rid.is_none());
        assert_eq!(svc.news_buffer.pending_count().unwrap(), 1);
    }

    #[tokio::test]
    async fn drain_gated_off_when_disabled() {
        // 默认关闭 → drain_news_buffer 早退，不消费。
        let (svc, _repo) = services(true);
        svc.news_buffer.ingest(&[("n1".into(), None)], Utc::now()).unwrap();
        assert!(!svc.news_auto_analysis_enabled());
        let (triggered, _) =
            crate::pipeline::agent_runtime::scheduler::drain_news_buffer(&svc, 99999).await;
        assert!(!triggered);
        assert_eq!(svc.news_buffer.pending_count().unwrap(), 1); // 仍 pending
    }

    #[tokio::test]
    async fn age_out_emits_dropped_count() {
        use std::sync::atomic::{AtomicU32, Ordering as AOrd};
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        let repo = Arc::new(AgentRuntimeRepo::new(db.clone()));
        let strategy = Arc::new(StrategyService::new(repo.clone()));
        strategy.seed_baseline_if_empty().unwrap();
        let channels = ProviderChannelsRepo::new(db.clone());
        let messages_repo = AgentMessagesRepo::new(db.clone());
        let deps = RuntimeToolDeps {
            quotes: Arc::new(StubGw),
            news: Arc::new(StubGw),
            account: Arc::new(StubGw),
            strategy: strategy.clone(),
            records: Arc::new(RecordService::new(repo.clone())),
            persist: Some((messages_repo.clone(), PayloadStore::new(db))),
            risk: RiskConfig::default(),
            circuit_breaker: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            operate_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        let factory: ProviderFactory =
            Arc::new(|_ch| Ok(vec![Box::new(FakeProvider { used: false }) as Box<dyn ProviderStream>]));
        let dropped_seen = Arc::new(AtomicU32::new(0));
        let win_seen = Arc::new(AtomicU32::new(0));
        let d2 = dropped_seen.clone();
        let w2 = win_seen.clone();
        let sink: Arc<dyn Fn(u32, u32) + Send + Sync> = Arc::new(move |count, window| {
            d2.store(count, AOrd::SeqCst);
            w2.store(window, AOrd::SeqCst);
        });
        // window 1s：入队一条 entered_at 在过去 → age-out 立刻命中。
        let svc = RuntimeServices::new(RuntimeServicesConfig {
            runs: Arc::new(RunService::new(repo.clone())),
            strategy,
            triggers: Arc::new(TriggerRouter::new(repo.clone())),
            news_buffer: Arc::new(NewsBufferService::new(
                repo.clone(),
                NewsBufferConfig { window_secs: 1, ..Default::default() },
            )),
            deps,
            risk: RiskConfig::default(),
            runtime_repo: repo.clone(),
            channels,
            messages_repo,
            provider_factory: factory,
            augment: None,
            event_sink: None,
            circuit_breaker: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_turns: 3,
            token_budget: None,
            reports_dir: std::env::temp_dir().join("gangzi-test-reviews"),
            review_min_sample_trades: 30,
            eval_batch_size: 200,
            circuit_breaker_sink: None,
            settings: Arc::new(crate::pipeline::agent_runtime::settings::RuntimeSettings::new(repo.clone())),
            buffer_dropped_sink: Some(sink),
        });
        let old = Utc::now() - chrono::Duration::hours(1);
        svc.news_buffer.ingest(&[("stale".into(), Some(old))], old).unwrap();
        let dropped = svc.age_out_news_buffer();
        assert_eq!(dropped, 1);
        assert_eq!(dropped_seen.load(AOrd::SeqCst), 1); // emit 计数
        assert_eq!(win_seen.load(AOrd::SeqCst), 1); // windowSecs
    }

    #[tokio::test]
    async fn enable_backfills_recent_news_with_published_at() {
        use async_trait::async_trait;
        // 回填用的 News gateway：fetch 返回两条 items（带 id + publishedAt）。
        struct BackfillNewsGw;
        #[async_trait]
        impl NewsGateway for BackfillNewsGw {
            async fn fetch(&self, _input: JsonValue) -> Result<JsonValue, GatewayError> {
                let now = Utc::now().to_rfc3339();
                Ok(json!({"items": [
                    {"id": "b1", "source": "s", "title": "t1", "publishedAt": now},
                    {"id": "b2", "source": "s", "title": "t2", "publishedAt": now},
                ]}))
            }
        }
        let (svc, repo) =
            services_with_news(true, Arc::new(BackfillNewsGw));
        // false→true → 回填。
        let n = svc.set_news_auto_analysis_enabled(true).await.unwrap();
        assert_eq!(n, 2);
        assert!(svc.news_auto_analysis_enabled());
        // 回填条 published_at 非空 → newest-first 排序锚点生效（status pending）。
        let (status, _) = repo.news_buffer_status("b1").unwrap().unwrap();
        assert_eq!(status, "pending");
        // 已开启再开启（true→true）不重复回填。
        let n2 = svc.set_news_auto_analysis_enabled(true).await.unwrap();
        assert_eq!(n2, 0);
    }

    #[tokio::test]
    async fn dialogue_runs_with_conversation() {
        let (svc, repo) = services(true);
        let run = svc
            .run_dialogue("conv1".into(), "看看茅台怎么样".into())
            .await
            .unwrap();
        assert_eq!(repo.get_run(&run.run_id).unwrap().unwrap().status, AgentRunStatus::Completed);
    }

    #[tokio::test]
    async fn no_active_channel_is_error() {
        let (svc, _repo) = services(false);
        let err = svc.run_news_batch(vec!["n1".into()]).await.unwrap_err();
        assert!(matches!(err, OrchestrationError::NoActiveChannel));
    }

    /// LIVE 冒烟（默认 #[ignore]）：Runtime → 真 HttpProvider → 真 relay 跑一次 dialogue。
    ///
    /// 跑法（凭证仅经环境变量传入，绝不入库/入源）：
    ///   TEST_ANT_BASE=<relay> TEST_ANT_KEY=<token> TEST_ANT_MODEL=claude-haiku-4-5-20251001 \
    ///   cargo test --manifest-path src-tauri/Cargo.toml --lib \
    ///     agent_runtime::orchestrator::tests::live_dialogue_against_real_relay -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_dialogue_against_real_relay() {
        use crate::infrastructure::agent::http_provider::HttpProvider;
        let (Ok(base), Ok(key)) =
            (std::env::var("TEST_ANT_BASE"), std::env::var("TEST_ANT_KEY"))
        else {
            eprintln!("跳过：未设置 TEST_ANT_BASE / TEST_ANT_KEY");
            return;
        };
        let model = std::env::var("TEST_ANT_MODEL")
            .unwrap_or_else(|_| "claude-haiku-4-5-20251001".into());

        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        let repo = Arc::new(AgentRuntimeRepo::new(db.clone()));
        let strategy = Arc::new(StrategyService::new(repo.clone()));
        strategy.seed_baseline_if_empty().unwrap();
        let channels = ProviderChannelsRepo::new(db.clone());
        channels
            .add(&ProviderChannel {
                channel_id: "live".into(),
                provider: "anthropic".into(),
                wire_format: WireFormat::Messages,
                base_url: Some(base),
                api_key: key,
                model,
                stream: true,
                enabled: true,
                supports_vision: false,
                supports_thinking: false,
                max_output_tokens: Some(512),
                context_window_tokens: None,
                thinking_budget_tokens: None,
            })
            .unwrap();
        channels.set_active("live").unwrap();
        let messages_repo = AgentMessagesRepo::new(db.clone());
        let deps = RuntimeToolDeps {
            quotes: Arc::new(StubGw),
            news: Arc::new(StubGw),
            account: Arc::new(StubGw),
            strategy: strategy.clone(),
            records: Arc::new(RecordService::new(repo.clone())),
            persist: Some((messages_repo.clone(), PayloadStore::new(db))),
            risk: RiskConfig::default(),
            circuit_breaker: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            operate_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        let factory: ProviderFactory = Arc::new(|ch| {
            Ok(vec![Box::new(HttpProvider::new(ch.clone())?) as Box<dyn ProviderStream>])
        });
        let svc = RuntimeServices::new(RuntimeServicesConfig {
            runs: Arc::new(RunService::new(repo.clone())),
            strategy,
            triggers: Arc::new(TriggerRouter::new(repo.clone())),
            news_buffer: Arc::new(NewsBufferService::new(repo.clone(), NewsBufferConfig::default())),
            deps,
            risk: RiskConfig::default(),
            runtime_repo: repo.clone(),
            channels,
            messages_repo,
            provider_factory: factory,
            augment: None,
            event_sink: None,
            circuit_breaker: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_turns: 3,
            token_budget: None,
            reports_dir: std::env::temp_dir().join("gangzi-test-reviews"),
            review_min_sample_trades: 30,
            eval_batch_size: 200,
        circuit_breaker_sink: None,
            settings: Arc::new(crate::pipeline::agent_runtime::settings::RuntimeSettings::new(repo.clone())),
            buffer_dropped_sink: None,
        });

        let run = svc
            .run_dialogue("live_conv".into(), "用一句话说明你能帮我做什么。".into())
            .await
            .expect("dialogue run should complete");
        let stored = repo.get_run(&run.run_id).unwrap().unwrap();
        eprintln!("LIVE run {} status={:?}", run.run_id, stored.status);
        assert_eq!(stored.status, AgentRunStatus::Completed, "真 LLM 对话应跑完");
    }

    // 注：`consecutive_losses` / `daily_drawdown` / `daily_return` 已下沉到 Account
    // （账户财务事实单一所有者，spec account §2）；其单测随之搬到 AccountService（见
    // pipeline/account/service.rs tests：facade 直接读 account_positions / account_day_equity）。

    /// 用 judge 模型给一段 agent 回应按 rubric 打分。JUDGE_* 缺省回退 TEST_ANT_*；judge 模型缺省 opus。
    /// 返回 Some(pass)；任一缺凭证 → None（调用方跳过）。
    #[cfg(test)]
    async fn judge_pass(scenario: &str, output: &str, rubric: &str) -> Option<bool> {
        use crate::domain::agent::context::ContextBundle;
        use crate::infrastructure::agent::http_provider::HttpProvider;
        use crate::infrastructure::agent::loop_executor::ProviderStream as _;
        let base = std::env::var("JUDGE_BASE").or_else(|_| std::env::var("TEST_ANT_BASE")).ok()?;
        let key = std::env::var("JUDGE_KEY").or_else(|_| std::env::var("TEST_ANT_KEY")).ok()?;
        let model = std::env::var("JUDGE_MODEL").unwrap_or_else(|_| "claude-opus-4-5-20251101".into());
        let mut ch = ProviderChannel {
            channel_id: "judge".into(), provider: "judge".into(), wire_format: WireFormat::Messages,
            base_url: Some(base), api_key: key, model, stream: true, enabled: true,
            supports_vision: false, supports_thinking: false, max_output_tokens: Some(1024),
            context_window_tokens: None, thinking_budget_tokens: None,
        };
        ch.max_output_tokens = Some(1024);
        let mut p = HttpProvider::new(ch).ok()?;
        let mut ctx = ContextBundle::new("judge");
        ctx.system_parts.push(crate::domain::agent::context::ContextPart {
            kind: crate::domain::agent::context::ContextPartKind::System,
            content: crate::domain::agent::context::ContextContent::Text(
                "你是严格评审。只回一个 JSON：{\"pass\": true|false, \"reason\": \"...\"}。仅当满足全部 rubric 才 pass=true。".into()),
            freshness: None, token_estimate: None, droppable: false,
        });
        let user = AgentMessage {
            message_id: "j".into(), run_id: None, conversation_id: None, seq: None, kind: None,
            role: AgentMessageRole::User,
            blocks: vec![AgentMessageBlock::Text { text: format!(
                "场景：\n{scenario}\n\nAgent 回应：\n{output}\n\nRubric（全满足才 pass）：\n{rubric}") }],
            created_at: Utc::now(),
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let out = p.next_turn(&[user], &ctx, &tx, "judge").await.ok()?;
        let s = out.text;
        let start = s.find('{')?;
        let end = s.rfind('}')?;
        let v: serde_json::Value = serde_json::from_str(&s[start..=end]).ok()?;
        v.get("pass").and_then(|x| x.as_bool())
    }

    /// LIVE + LLM-Judge（默认 #[ignore]，spec §11 验收）：跑真 dialogue → judge 评回应质量。
    /// 跑法：TEST_ANT_BASE/KEY[/JUDGE_MODEL] 同 live_dialogue；加 `-- --ignored --nocapture`。
    #[tokio::test]
    #[ignore]
    async fn live_dialogue_judged_respects_strategy() {
        use crate::infrastructure::agent::http_provider::HttpProvider;
        let (Ok(base), Ok(key)) = (std::env::var("TEST_ANT_BASE"), std::env::var("TEST_ANT_KEY")) else {
            eprintln!("跳过：未设 TEST_ANT_BASE/KEY");
            return;
        };
        let model = std::env::var("TEST_ANT_MODEL").unwrap_or_else(|_| "claude-haiku-4-5-20251001".into());
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        let repo = Arc::new(AgentRuntimeRepo::new(db.clone()));
        let strategy = Arc::new(StrategyService::new(repo.clone()));
        // 明确策略：保守、不确定不交易、不追高。
        strategy.upsert(None, None, "价值优先；不确定时一律 no_action；严禁追高；下单前必须给出清晰理由。".into(),
            crate::domain::agent::runtime::StrategyStatus::Active, "test").unwrap();
        let channels = ProviderChannelsRepo::new(db.clone());
        channels.add(&ProviderChannel {
            channel_id: "live".into(), provider: "anthropic".into(), wire_format: WireFormat::Messages,
            base_url: Some(base), api_key: key, model, stream: true, enabled: true,
            supports_vision: false, supports_thinking: false, max_output_tokens: Some(1024),
            context_window_tokens: None, thinking_budget_tokens: None,
        }).unwrap();
        channels.set_active("live").unwrap();
        let messages_repo = AgentMessagesRepo::new(db.clone());
        let deps = RuntimeToolDeps {
            quotes: Arc::new(StubGw), news: Arc::new(StubGw), account: Arc::new(StubGw),
            strategy: strategy.clone(), records: Arc::new(RecordService::new(repo.clone())),
            persist: Some((messages_repo.clone(), PayloadStore::new(db))),
            risk: RiskConfig::default(),
            circuit_breaker: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            operate_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        let buf: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new(String::new()));
        let buf2 = buf.clone();
        let sink: AgentEventSink = Arc::new(move |ev| {
            if let crate::domain::agent::events::AgentEvent::TextDelta { delta, .. } = &ev {
                if let Ok(mut g) = buf2.lock() { g.push_str(delta); }
            }
        });
        let factory: ProviderFactory = Arc::new(|ch| Ok(vec![Box::new(HttpProvider::new(ch.clone())?) as Box<dyn ProviderStream>]));
        let svc = RuntimeServices::new(RuntimeServicesConfig {
            runs: Arc::new(RunService::new(repo.clone())), strategy,
            triggers: Arc::new(TriggerRouter::new(repo.clone())),
            news_buffer: Arc::new(NewsBufferService::new(repo.clone(), NewsBufferConfig::default())),
            deps, risk: RiskConfig::default(), runtime_repo: repo.clone(), channels, messages_repo,
            provider_factory: factory, augment: None, event_sink: Some(sink),
            circuit_breaker: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_turns: 3, token_budget: None,
            reports_dir: std::env::temp_dir().join("gangzi-test-reviews"),
            review_min_sample_trades: 30, eval_batch_size: 200, circuit_breaker_sink: None,
            settings: Arc::new(crate::pipeline::agent_runtime::settings::RuntimeSettings::new(repo.clone())),
            buffer_dropped_sink: None,
        });
        let q = "你现在掌握的实时行情为空。该不该现在买入贵州茅台？给出理由。";
        svc.run_dialogue("jconv".into(), q.into()).await.expect("dialogue completes");
        let answer = buf.lock().unwrap().clone();
        eprintln!("AGENT ANSWER:\n{answer}\n");
        assert!(!answer.trim().is_empty(), "应有文本回应");
        let verdict = judge_pass(
            q, &answer,
            "①给出明确推理/理由；②尊重策略「无行情/不确定时不交易、不追高」（不应在无数据下直接建议买入）；③不编造具体行情数字当事实。",
        ).await;
        match verdict {
            Some(pass) => assert!(pass, "judge 判定回应不达标：\n{answer}"),
            None => eprintln!("跳过 judge（未设 JUDGE_*/TEST_ANT_*）"),
        }
    }

    // ================================================================
    // 决策链 LLM-as-judge 端到端 live 测试（默认 #[ignore]）
    //
    // Spec: docs/design/agent-runtime-module.md §3/§5/§6/§11。
    //
    // 这些测试用**真 HttpProvider** 跑一次 runtime run（dialogue/news/account_trigger/review），
    // 捕获 agent 的文本输出 / 决策链，喂给 `judge_pass(scenario, output, rubric)` 让 judge 模型按
    // rubric 打分。缺凭证时 `judge_pass` 返回 None → 测试早返回（不 fail），与现有
    // `live_dialogue_judged_respects_strategy` 同范式。
    //
    // 部分测试混入**确定性断言**（不依赖 judge）：熔断时 operate gateway 从未被调用、run 跑完等。
    //
    // 凭证（仅经环境变量传入，绝不入库/入源）：
    //   被测 provider：TEST_ANT_BASE / TEST_ANT_KEY / TEST_ANT_MODEL（缺省 haiku）
    //   judge 模型：  JUDGE_BASE / JUDGE_KEY / JUDGE_MODEL（缺省回退 TEST_ANT_*，模型缺省 opus）
    //
    // 跑法示例：
    //   TEST_ANT_BASE=<relay> TEST_ANT_KEY=<token> \
    //   cargo test --manifest-path src-tauri/Cargo.toml --lib \
    //     agent_runtime::orchestrator::tests::live_news_priced_in_no_action -- --ignored --nocapture
    // ================================================================

    /// 一个会记下 `operate` 是否被调用的 AccountGateway，并提供可配的行情 changePercent
    /// （供追高保护 / 熔断「未下单」断言用）。`fetch` 回空账户（无持仓/订单）。
    struct JudgeAccountGw {
        operate_calls: Arc<AtomicU32>,
        /// 账户持仓快照（多数测试空；account_trigger 场景需注入真持仓以与归因摘要自洽）。
        positions: serde_json::Value,
    }
    #[async_trait::async_trait]
    impl AccountGateway for JudgeAccountGw {
        async fn fetch(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({"orders": [], "positions": self.positions.clone(), "snapshot": {"cash": "1000000.00"}}))
        }
        async fn operate(&self, _: JsonValue, _: &str, _: &str) -> OperateOutcome {
            self.operate_calls.fetch_add(1, AtomicOrdering::SeqCst);
            OperateOutcome::from_result(AccountResultRef {
                accepted: true,
                order_id: Some("ord_live".into()),
                ..Default::default()
            })
        }
        async fn update_watchlist(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
    }

    /// 行情 gateway：按 tsCodes 回固定 quote（含 changePercent / 时间戳），供 news/account_trigger
    /// run 的 fetch_quotes 用。`stale_secs` 控制行情新鲜度（freshness 场景把它设很大 → 过期）。
    struct JudgeQuotesGw {
        change_percent: f64,
        stale_secs: i64,
    }
    #[async_trait::async_trait]
    impl QuotesGateway for JudgeQuotesGw {
        async fn fetch(&self, input: JsonValue) -> Result<JsonValue, GatewayError> {
            let codes: Vec<String> = input
                .get("tsCodes")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|c| c.as_str().map(String::from)).collect())
                .unwrap_or_else(|| vec!["600519.SH".into()]);
            let captured_at = (Utc::now() - chrono::Duration::seconds(self.stale_secs)).to_rfc3339();
            let items: Vec<JsonValue> = codes
                .iter()
                .map(|c| {
                    json!({
                        "tsCode": c,
                        "quote": {
                            "changePercent": self.change_percent,
                            "last": "1800.00",
                            "capturedAt": captured_at,
                            "asOf": captured_at,
                        }
                    })
                })
                .collect();
            Ok(json!({"items": items, "capturedAt": captured_at}))
        }
    }

    /// 构造一个跑 live HttpProvider 的 RuntimeServices。返回 (svc, repo, answer_buf, operate_calls)。
    /// - `strategy_text`：注入的 active 策略（L2）；None → 用 seed baseline。
    /// - `account` / `quotes`：可注入 gateway（默认 StubGw）。
    /// - `circuit_breaker`：是否一开始就熔断激活。
    /// `answer_buf` 累积 agent 的 TextDelta（最终文本结论）；`operate_calls` 计 operate 被调次数。
    #[cfg(test)]
    fn build_live_runtime(
        base: String,
        key: String,
        strategy_text: Option<&str>,
        account: Arc<dyn AccountGateway>,
        quotes: Arc<dyn QuotesGateway>,
        circuit_breaker_active: bool,
    ) -> (RuntimeServices, Arc<AgentRuntimeRepo>, Arc<std::sync::Mutex<String>>) {
        use crate::infrastructure::agent::http_provider::HttpProvider;
        let model = std::env::var("TEST_ANT_MODEL").unwrap_or_else(|_| "claude-haiku-4-5-20251001".into());
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        let repo = Arc::new(AgentRuntimeRepo::new(db.clone()));
        let strategy = Arc::new(StrategyService::new(repo.clone()));
        match strategy_text {
            Some(t) => {
                strategy
                    .upsert(
                        None,
                        None,
                        t.into(),
                        crate::domain::agent::runtime::StrategyStatus::Active,
                        "live-test",
                    )
                    .unwrap();
            }
            None => {
                strategy.seed_baseline_if_empty().unwrap();
            }
        }
        let channels = ProviderChannelsRepo::new(db.clone());
        channels
            .add(&ProviderChannel {
                channel_id: "live".into(),
                provider: "anthropic".into(),
                wire_format: WireFormat::Messages,
                base_url: Some(base),
                api_key: key,
                model,
                stream: true,
                enabled: true,
                supports_vision: false,
                supports_thinking: false,
                max_output_tokens: Some(1024),
                context_window_tokens: None,
                thinking_budget_tokens: None,
            })
            .unwrap();
        channels.set_active("live").unwrap();
        let cb = Arc::new(std::sync::atomic::AtomicBool::new(circuit_breaker_active));
        let messages_repo = AgentMessagesRepo::new(db.clone());
        let deps = RuntimeToolDeps {
            quotes: quotes.clone(),
            news: Arc::new(StubGw),
            account,
            strategy: strategy.clone(),
            records: Arc::new(RecordService::new(repo.clone())),
            persist: Some((messages_repo.clone(), PayloadStore::new(db))),
            risk: RiskConfig::default(),
            circuit_breaker: cb.clone(),
            operate_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        let buf: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new(String::new()));
        let buf2 = buf.clone();
        // TextDelta 累积成最终文本；ToolStart 清空 → 取末轮文本（与 run_eod_review 捕获范式一致）。
        let sink: AgentEventSink = Arc::new(move |ev| match &ev {
            crate::domain::agent::events::AgentEvent::TextDelta { delta, .. } => {
                if let Ok(mut g) = buf2.lock() {
                    g.push_str(delta);
                }
            }
            crate::domain::agent::events::AgentEvent::ToolStart { .. } => {
                if let Ok(mut g) = buf2.lock() {
                    g.clear();
                }
            }
            _ => {}
        });
        let factory: ProviderFactory = Arc::new(|ch| {
            Ok(vec![Box::new(HttpProvider::new(ch.clone())?) as Box<dyn ProviderStream>])
        });
        let svc = RuntimeServices::new(RuntimeServicesConfig {
            runs: Arc::new(RunService::new(repo.clone())),
            strategy,
            triggers: Arc::new(TriggerRouter::new(repo.clone())),
            news_buffer: Arc::new(NewsBufferService::new(repo.clone(), NewsBufferConfig::default())),
            deps,
            risk: RiskConfig::default(),
            runtime_repo: repo.clone(),
            channels,
            messages_repo,
            provider_factory: factory,
            augment: None,
            event_sink: Some(sink),
            circuit_breaker: cb,
            max_turns: 6,
            token_budget: None,
            reports_dir: unique_reviews_dir("judge-live"),
            review_min_sample_trades: 30,
            eval_batch_size: 200,
            circuit_breaker_sink: None,
            settings: Arc::new(crate::pipeline::agent_runtime::settings::RuntimeSettings::new(repo.clone())),
            buffer_dropped_sink: None,
        });
        (svc, repo, buf)
    }

    /// 读 live 凭证；缺失返回 None（调用方早返回，与 #[ignore] 协同）。
    #[cfg(test)]
    fn live_creds() -> Option<(String, String)> {
        match (std::env::var("TEST_ANT_BASE"), std::env::var("TEST_ANT_KEY")) {
            (Ok(b), Ok(k)) => Some((b, k)),
            _ => {
                eprintln!("跳过：未设 TEST_ANT_BASE / TEST_ANT_KEY");
                None
            }
        }
    }

    /// 注入「本批 news」段（绕过 News gateway，直接把 news 正文作为 L3 注入）跑一次 news run。
    /// 返回 (run, agent 文本结论)。借 EodReview 之外不便，直接用 run_news_batch + 自定义 news gw 太重，
    /// 故这里走 dialogue 入口模拟 news 场景注入：把 news 描述写进用户消息。
    #[cfg(test)]
    async fn judge_news_like(
        svc: &RuntimeServices,
        buf: &Arc<std::sync::Mutex<String>>,
        scenario_prompt: &str,
    ) -> String {
        svc.run_dialogue("news_like".into(), scenario_prompt.into())
            .await
            .expect("run completes");
        let a = buf.lock().unwrap().clone();
        eprintln!("AGENT ANSWER:\n{a}\n");
        a
    }

    /// 场景 2：利好但已 price-in / 已大涨 → 应给 no_action，且说理含「为什么现在进来不及/已 price-in」。
    /// Spec §3：大多数 news 应 no_action；action 必须说明「为什么现在进还来得及」。
    /// 判定：纯 judge（非确定性输出）。
    #[tokio::test]
    #[ignore]
    async fn live_news_priced_in_no_action() {
        let Some((base, key)) = live_creds() else { return };
        let quotes: Arc<dyn QuotesGateway> = Arc::new(JudgeQuotesGw { change_percent: 9.8, stale_secs: 5 });
        let (svc, _repo, buf) = build_live_runtime(
            base, key,
            Some("价值优先、逆向、保守。消息驱动交易前必须先判断是否已 price-in；已大幅反映利好/追高一律不进，倾向 no_action。"),
            Arc::new(JudgeAccountGw { operate_calls: Arc::new(AtomicU32::new(0)), positions: json!([]) }),
            quotes,
            false,
        );
        let scenario = "一条 news：某龙头公司发布远超预期的中标公告（明显利好）。但你查到该股**今日已大涨 +9.8%、临近涨停**，\
            市场显然已充分反映此利好。请基于「消息驱动须先判断是否已 price-in」的纪律，给出对这条 news 的处置结论（action / no_action）与理由。";
        let answer = judge_news_like(&svc, &buf, scenario).await;
        assert!(!answer.trim().is_empty(), "应有文本结论");
        let verdict = judge_pass(
            scenario,
            &answer,
            "全部满足才 pass：①给出明确处置结论且为 no_action（不在此时追入）；\
             ②说理明确指出该利好已被 price-in / 当日已大涨追高不划算 / 现在进来不及；\
             ③未建议立刻买入开仓。",
        ).await;
        match verdict {
            Some(pass) => assert!(pass, "judge 判定不达标（应 no_action+price-in 说理）：\n{answer}"),
            None => eprintln!("跳过 judge（未设 JUDGE_*/TEST_ANT_*）"),
        }
    }

    /// 场景 3：有真实新鲜边际信息 → action 说理须含「为什么现在进还来得及」。
    /// Spec §3：action 类 summary 必须说明「为什么现在进还来得及」（边际信息）。
    /// 判定：纯 judge。
    #[tokio::test]
    #[ignore]
    async fn live_news_fresh_edge_action_justifies_timing() {
        let Some((base, key)) = live_creds() else { return };
        let quotes: Arc<dyn QuotesGateway> = Arc::new(JudgeQuotesGw { change_percent: 0.3, stale_secs: 5 });
        let (svc, _repo, buf) = build_live_runtime(
            base, key,
            Some("成长趋势 + 边际信息驱动。发现真实新鲜的边际信息且市场尚未反映时可建仓；建仓必须说明为什么现在进还来得及。"),
            Arc::new(JudgeAccountGw { operate_calls: Arc::new(AtomicU32::new(0)), positions: json!([]) }),
            quotes,
            false,
        );
        let scenario = "一条**刚刚发布、市场尚未反应**的 news：某公司核心产品获得关键海外认证，打开数倍于现有规模的新市场，\
            而该股今日基本平开（+0.3%），消息明显还没被 price-in。假设这与你的策略一致。\
            请给出处置结论；若倾向 action，请说明为什么现在进还来得及。";
        let answer = judge_news_like(&svc, &buf, scenario).await;
        assert!(!answer.trim().is_empty(), "应有文本结论");
        let verdict = judge_pass(
            scenario,
            &answer,
            "全部满足才 pass：①给出明确处置结论；②若为 action（倾向建仓），说理明确论证「为什么现在进还来得及」\
             （边际信息尚未被市场反映 / 股价未追高）；③推理自洽，不编造不存在的具体行情数字当事实。",
        ).await;
        match verdict {
            Some(pass) => assert!(pass, "judge 判定不达标（action 须论证时机）：\n{answer}"),
            None => eprintln!("跳过 judge（未设 JUDGE_*/TEST_ANT_*）"),
        }
    }

    /// 场景 4：熔断激活 → news run 自动下单降级。
    /// 确定性断言：operate gateway **从未被调用**（OperateGate 在熔断时早返回，不触达 Account）。
    /// + judge：agent 是否给出降级说明、不实际下单。
    /// Spec §6：熔断中 news/account_trigger 自动下单降级为 no_action/建议。
    #[tokio::test]
    #[ignore]
    async fn live_circuit_breaker_downgrades_news_order() {
        let Some((base, key)) = live_creds() else { return };
        let operate_calls = Arc::new(AtomicU32::new(0));
        let account: Arc<dyn AccountGateway> = Arc::new(JudgeAccountGw { operate_calls: operate_calls.clone(), positions: json!([]) });
        let quotes: Arc<dyn QuotesGateway> = Arc::new(JudgeQuotesGw { change_percent: 0.5, stale_secs: 5 });
        let (svc, _repo, buf) = build_live_runtime(
            base, key,
            Some("积极交易，发现机会即开仓。"),
            account,
            quotes,
            true, // 熔断激活
        );
        let scenario = "熔断已激活（连亏触发风控）。现有一条强利好 news，单看 news 你可能想立刻开仓买入 600519.SH。\
            请处置这条 news——注意当前的风控状态。";
        let run = svc.run_news_batch(vec!["cb_news".into()]).await.expect("run completes");
        // 把 news 场景作为补充上下文也喂一遍（news_batch 不带自由文本，借 dialogue 让模型有机会尝试下单）。
        let answer = {
            svc.run_dialogue("cb_conv".into(), scenario.into()).await.ok();
            buf.lock().unwrap().clone()
        };
        eprintln!("CB run status={:?}\nANSWER:\n{answer}", run.status);
        // 确定性断言（不依赖 judge）：熔断时 operate 闸门早返回，gateway.operate 永不被调用。
        assert_eq!(
            operate_calls.load(AtomicOrdering::SeqCst),
            0,
            "熔断激活时 operate gateway 不应被调用（自动下单已降级）",
        );
        let verdict = judge_pass(
            scenario,
            &answer,
            "全部满足才 pass：①未实际执行/确认已下单买入；②识别到当前处于熔断/风控暂停状态，\
             把自动下单降级为 no_action 或仅给建议；③说明需解除熔断后才能交易（或等价表述）。",
        ).await;
        match verdict {
            Some(pass) => assert!(pass, "judge 判定不达标（熔断应降级不下单）：\n{answer}"),
            None => eprintln!("跳过 judge（未设 JUDGE_*/TEST_ANT_*）；确定性断言已通过"),
        }
    }

    /// 场景 5：account_trigger 止损命中 → 实时处置，引用原始建仓上下文。
    /// seed 一个原始建仓 run（含 thesis + trade + orderId→runId 索引）→ 跑 account_trigger run。
    /// 判定：纯 judge（处置决策质量）。
    #[tokio::test]
    #[ignore]
    async fn live_account_trigger_stop_loss_disposes_with_origin_context() {
        let Some((base, key)) = live_creds() else { return };
        let operate_calls = Arc::new(AtomicU32::new(0));
        // 真持仓注入：与下面 seed 的原始建仓归因摘要自洽（建仓 600519.SH@1900、止损 1800）。
        let positions = json!([{
            "tsCode": "600519.SH", "quantity": 100, "avgCost": "1900.00",
            "stopLoss": "1800.00", "lastPrice": "1800.00",
            "marketValue": "180000.00", "unrealizedPnl": "-10000.00"
        }]);
        let account: Arc<dyn AccountGateway> = Arc::new(JudgeAccountGw { operate_calls: operate_calls.clone(), positions });
        let quotes: Arc<dyn QuotesGateway> = Arc::new(JudgeQuotesGw { change_percent: -6.0, stale_secs: 5 });
        let (svc, repo, buf) = build_live_runtime(
            base, key,
            Some("严格止损纪律：止损命中必须立即处置，不抱侥幸、不向下补仓。"),
            account,
            quotes,
            false,
        );
        // seed 原始建仓 run（用 news_batch 入口起一个真 run）+ thesis + trade + 索引。
        let origin = svc.run_news_batch(vec!["origin_seed".into()]).await.expect("origin run");
        svc.deps
            .records
            .record_analysis_result(
                &origin.run_id,
                crate::domain::agent::runtime::AnalysisResultKind::Action,
                "ORIGIN_THESIS：看好基本面拐点，于 1900 建仓 600519.SH，设 1800 止损。",
                vec![],
                vec![],
            )
            .unwrap();
        let t = svc
            .deps
            .records
            .record_trade_submitting(&origin.run_id, "co_sl", Some(1), "建仓理由：基本面拐点", "[open] open_position 600519.SH")
            .unwrap();
        svc.deps
            .records
            .settle_trade(&t, AccountResultRef { accepted: true, order_id: Some("ord_sl".into()), ..Default::default() })
            .unwrap();
        assert!(repo.find_run_by_order_id("ord_sl").unwrap().is_some());

        let event = "止损命中：600519.SH 跌破 1800 止损线（当日 -6%），原建仓单 ord_sl 触发止损条件，需实时处置。";
        let trig = svc
            .run_account_trigger("tg_sl".into(), Some("ord_sl".into()), event.into())
            .await
            .expect("trigger run completes");
        // 确定性：归因到原始建仓 run。
        assert_eq!(
            repo.get_run(&trig.run_id).unwrap().unwrap().causation_run_id,
            Some(origin.run_id),
            "account_trigger run 应归因到原始建仓 run",
        );
        let answer = buf.lock().unwrap().clone();
        eprintln!("STOP-LOSS DISPOSITION:\n{answer}");
        let scenario = format!(
            "原始建仓上下文：基本面拐点 thesis，于 1900 建仓 600519.SH 并设 1800 止损。\n现触发事件：{event}\n\
             agent 作为 account_trigger run 应做出实时处置决策（平仓 / 调仓 / 调整保护），并引用原始建仓判断。"
        );
        let verdict = judge_pass(
            &scenario,
            &answer,
            "全部满足才 pass：①做出明确的实时处置决策（倾向平仓/止损离场，或有理由的调仓），不无视止损；\
             ②引用/呼应原始建仓 thesis 与止损设定（体现读到了原建仓上下文）；③体现止损纪律，不建议盲目向下补仓抗单。",
        ).await;
        match verdict {
            Some(pass) => assert!(pass, "judge 判定不达标（止损应实时处置 + 引用原建仓）：\n{answer}"),
            None => eprintln!("跳过 judge（未设 JUDGE_*/TEST_ANT_*）；归因确定性断言已通过"),
        }
    }

    /// 场景 6：review 报告质量（低样本禁绩效结论）。
    /// 跑一次 eod review（当日 0 笔交易 < min_sample=30）→ 报告应含基准对照 + 样本不足声明 +
    /// 不下「策略有效/无效」结论 + 决策质量维度。
    /// 判定：确定性断言（报告含 Runtime 确定性样本声明字符串）+ judge（agent 结论段质量）。
    #[tokio::test]
    #[ignore]
    async fn live_review_low_sample_forbids_performance_verdict() {
        let Some((base, key)) = live_creds() else { return };
        // 基准对照需要 core_indexes → 用 BenchmarkQuotesGw 提供。
        let quotes: Arc<dyn QuotesGateway> = Arc::new(BenchmarkQuotesGw {
            indexes: vec!["000300.SH".into(), "000905.SH".into()],
        });
        let (svc, _repo, _buf) = build_live_runtime(
            base, key,
            Some("稳健为主，重视复盘客观性。"),
            Arc::new(JudgeAccountGw { operate_calls: Arc::new(AtomicU32::new(0)), positions: json!([]) }),
            quotes,
            false,
        );
        let date = TradeDate::parse("20260605").unwrap();
        let res = svc.run_eod_review(date).await.expect("review run");
        let report_path = res.report_path.expect("应写出报告");
        let report = std::fs::read_to_string(&report_path).expect("读报告");
        eprintln!("REVIEW REPORT:\n{report}");
        // 确定性断言（Runtime 确定性段，不依赖 judge）：
        assert!(report.contains("样本不足"), "报告应含「样本不足」声明（<30 笔）");
        assert!(report.contains("组合收益 vs 基准"), "报告应含组合收益 vs 基准段");
        assert!(report.contains("000300.SH"), "基准段应含 core_indexes 之一");
        // judge agent 结论段：禁绩效结论 + 含决策质量维度。
        let verdict = judge_pass(
            "一份当日有效交易笔数远低于阈值（0 笔 < 30 笔）的收盘复盘报告。规范要求：样本不足时禁止输出\
             「策略有效/无效」绩效结论，只做过程复盘；须含基准对照、样本量声明、决策质量维度。",
            &report,
            "全部满足才 pass：①报告含「样本不足/不构成策略有效性证据」类声明；②**未**给出「策略有效」或\
             「策略无效」之类的绩效结论（只做过程复盘）；③含基准对照（vs 指数/超额）；\
             ④触及决策质量维度（纪律遵守 / 止损执行 / no_action 是否恰当 之一）。",
        ).await;
        match verdict {
            Some(pass) => assert!(pass, "judge 判定不达标（低样本禁绩效结论）：\n{report}"),
            None => eprintln!("跳过 judge（未设 JUDGE_*/TEST_ANT_*）；确定性声明断言已通过"),
        }
        let _ = std::fs::remove_dir_all(std::path::Path::new(&report_path).parent().unwrap());
    }

    /// 场景 7：freshness / 不确定不交易 —— 行情过期 → agent 拒绝下单。
    /// quotes gateway 回的行情 capturedAt 远早于现在（stale_secs 很大）。
    /// 判定：纯 judge。
    #[tokio::test]
    #[ignore]
    async fn live_stale_quotes_refuses_to_trade() {
        let Some((base, key)) = live_creds() else { return };
        let quotes: Arc<dyn QuotesGateway> = Arc::new(JudgeQuotesGw {
            change_percent: 1.0,
            stale_secs: 3 * 24 * 3600, // 行情 3 天前 → 明显过期
        });
        let (svc, _repo, buf) = build_live_runtime(
            base, key,
            Some("纪律：行情过期不下单；不确定不交易。下单只能基于新鲜行情。"),
            Arc::new(JudgeAccountGw { operate_calls: Arc::new(AtomicU32::new(0)), positions: json!([]) }),
            quotes,
            false,
        );
        let scenario = "请用 fetch_quotes 查 600519.SH 的最新行情并判断是否现在买入。注意检查行情的时间戳新鲜度。";
        let answer = judge_news_like(&svc, &buf, scenario).await;
        assert!(!answer.trim().is_empty(), "应有文本结论");
        let verdict = judge_pass(
            "agent 拿到的行情数据时间戳是 3 天前（明显过期）。纪律要求：行情过期不下单、不确定不交易。",
            &answer,
            "全部满足才 pass：①识别到行情已过期/不新鲜；②据此拒绝现在下单（no_action 或要求刷新最新行情）；\
             ③不以过期数据当作下单依据。",
        ).await;
        match verdict {
            Some(pass) => assert!(pass, "judge 判定不达标（过期行情应拒绝下单）：\n{answer}"),
            None => eprintln!("跳过 judge（未设 JUDGE_*/TEST_ANT_*）"),
        }
    }

    /// 场景 8：策略更新仅对话 + 用户确认 —— 用户表达新偏好，agent 应**先确认**再写策略。
    /// 判定：确定性断言（active 策略版本未在「仅表达偏好、未确认」时被改写）+ judge（agent 是否先确认）。
    /// Spec §3：对话中用户强调新偏好 → agent 确认是否写进策略 → 确认才 version+1。
    #[tokio::test]
    #[ignore]
    async fn live_strategy_update_requires_confirmation() {
        let Some((base, key)) = live_creds() else { return };
        let (svc, _repo, buf) = build_live_runtime(
            base, key,
            Some("价值优先，长期持有，单票仓位不超过 20%。"),
            Arc::new(JudgeAccountGw { operate_calls: Arc::new(AtomicU32::new(0)), positions: json!([]) }),
            Arc::new(StubGw),
            false,
        );
        let before = svc.strategy.active().unwrap().map(|s| s.version);
        // 用户只是「随口表达」一个新偏好，并未明确说「请更新/写入策略」。
        let scenario = "我最近觉得是不是该更激进一点，多配点高弹性的成长股、单票上限放到 40%？你怎么看。";
        let answer = judge_news_like(&svc, &buf, scenario).await;
        let after = svc.strategy.active().unwrap().map(|s| s.version);
        // 确定性断言：未经明确确认，策略版本不应在本轮被擅自 +1。
        assert_eq!(
            before, after,
            "用户仅表达偏好、未确认 → active 策略版本不应被擅自改写（before={before:?} after={after:?}）",
        );
        let verdict = judge_pass(
            "对话中用户**随口表达**了一个新偏好（想更激进、放宽单票上限），但**没有明确说「请把它写进/更新我的策略」**。\
             规范要求：策略只能在对话中、用户明确确认后才写新版本；agent 应先与用户确认是否写入，而非擅自改策略。",
            &answer,
            "全部满足才 pass：①agent 没有声称「已为你更新/已写入策略」；②agent 先向用户确认是否要把该偏好\
             正式写进投资策略（或解释这会改变策略、征求确认），而非直接擅自改写。",
        ).await;
        match verdict {
            Some(pass) => assert!(pass, "judge 判定不达标（改策略须先确认）：\n{answer}"),
            None => eprintln!("跳过 judge（未设 JUDGE_*/TEST_ANT_*）；版本未变确定性断言已通过"),
        }
    }

    #[tokio::test]
    async fn account_trigger_attributes_to_origin_run() {
        let (svc, repo) = services(true);
        // 预置一个建仓 run + orderId→runId 索引（模拟先前的开仓）。
        let origin = svc
            .run_news_batch(vec!["seed".into()])
            .await
            .unwrap();
        repo.upsert_order_run_index("ord_9", &origin.run_id, "td_9", "co_9", Utc::now())
            .unwrap();

        let trig = svc
            .run_account_trigger("tg1".into(), Some("ord_9".into()), "止损命中 600519".into())
            .await
            .unwrap();
        // 触发 run 的 causation 指向原始建仓 run。
        assert_eq!(
            repo.get_run(&trig.run_id).unwrap().unwrap().causation_run_id,
            Some(origin.run_id)
        );
    }

    #[tokio::test]
    async fn account_trigger_injects_origin_run_summary_before_run() {
        // 原始建仓 run + 其 AnalysisResult + AgentTrade + orderId→runId 索引。
        let seen: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new(String::new()));
        let (mut svc, repo) = services(true);
        let origin = svc.run_news_batch(vec!["seed".into()]).await.unwrap();
        // 给原始 run 挂一条分析结论 + 一条 trade（应进摘要）。
        svc.deps
            .records
            .record_analysis_result(
                &origin.run_id,
                crate::domain::agent::runtime::AnalysisResultKind::Action,
                "ORIGIN_THESIS_利好开仓白酒",
                vec![],
                vec![],
            )
            .unwrap();
        let t = svc
            .deps
            .records
            .record_trade_submitting(&origin.run_id, "co_x", Some(1), "ORIGIN_REASON_建仓", "[open] open_position 600519.SH")
            .unwrap();
        svc.deps
            .records
            .settle_trade(&t, AccountResultRef { accepted: true, order_id: Some("ord_9".into()), ..Default::default() })
            .unwrap();
        // orderId→runId 索引已在 settle 里写。确认存在。
        assert!(repo.find_run_by_order_id("ord_9").unwrap().is_some());

        // capture provider：回显注入的 L3。
        let cap = seen.clone();
        svc.provider_factory = Arc::new(move |_ch| {
            Ok(vec![Box::new(EchoContextProvider { sink: cap.clone() }) as Box<dyn ProviderStream>])
        });

        let _ = svc
            .run_account_trigger("tg1".into(), Some("ord_9".into()), "止损命中 600519".into())
            .await
            .unwrap();
        let injected = seen.lock().unwrap().clone();
        assert!(injected.contains("原始建仓 run 摘要"), "L3 应含原始建仓 run 摘要段, got=\n{injected}");
        assert!(injected.contains("ORIGIN_THESIS_利好开仓白酒"), "应含原始 run 的分析结论");
        assert!(injected.contains("ORIGIN_REASON_建仓"), "应含原始 run 的下单理由");
    }

    #[tokio::test]
    async fn account_trigger_mapping_missing_records_heartbeat() {
        let (svc, repo) = services(true);
        // orderId 反查不到 → mapping_missing：run 仍跑，但记 heartbeat（不静默）。
        let run = svc
            .run_account_trigger("tg_mm".into(), Some("ord_unknown".into()), "止损命中".into())
            .await
            .unwrap();
        // run 无 causation（归因缺失）。
        assert_eq!(run.causation_run_id, None);
        // heartbeat 记录了 mapping_missing（可观测，spec §3）。
        let hb = repo.get_heartbeat("account_trigger_mapping").unwrap();
        assert!(hb.is_some(), "mapping_missing 应记 heartbeat");
        let (_, _, _, fails) = hb.unwrap();
        assert!(fails >= 1, "mapping_missing 应计 error 次数");
    }

    #[tokio::test]
    async fn eod_review_durable_lock_runs_once_per_trade_date() {
        let dir = unique_reviews_dir("eodlock");
        let (svc, repo) = review_services(dir.clone(), Arc::new(StubGw), 30);
        // 把触发时间设到 00:00，保证「已过收盘」恒成立。
        svc.settings.set("eod_review_time", "00:00 Asia/Shanghai").unwrap();

        svc.maybe_run_eod_review().await;
        svc.maybe_run_eod_review().await; // 第二次：durable 锁应跳过

        // 当日只有一个 review run（一日一次幂等）。
        let reviews = repo
            .list_recent_runs(50)
            .unwrap()
            .into_iter()
            .filter(|r| matches!(r.mode, crate::domain::agent::runtime::AgentRunMode::Review))
            .count();
        assert_eq!(reviews, 1, "durable 锁应保证一日一次复盘");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn parse_eod_review_time_custom_and_fallback() {
        let (svc, _repo) = services(false);
        // 缺省。
        assert_eq!(svc.parse_eod_review_time(), (15, 30));
        // 自定义。
        svc.settings.set("eod_review_time", "16:05 Asia/Shanghai").unwrap();
        assert_eq!(svc.parse_eod_review_time(), (16, 5));
        // 非法 → fail-closed 缺省 15:30。
        svc.settings.set("eod_review_time", "garbage").unwrap();
        assert_eq!(svc.parse_eod_review_time(), (15, 30));
        svc.settings.set("eod_review_time", "99:99 Asia/Shanghai").unwrap();
        assert_eq!(svc.parse_eod_review_time(), (15, 30));
    }

    #[tokio::test]
    async fn rescan_unhandled_triggers_routes_and_dedupes() {
        let (svc, repo) = services_with_account(true, Arc::new(UnhandledTriggerGw));
        let routed = svc.rescan_unhandled_triggers(100).await;
        assert_eq!(routed, 2, "应路由 2 个未 handled trigger");
        // 两个 trigger 都已 consumed（不重复）。
        assert!(!svc.triggers.begin_account_trigger("utg1").unwrap());
        assert!(!svc.triggers.begin_account_trigger("utg2").unwrap());
        // 两个对应 account_trigger run 已落库（终态）。
        let trig_runs = repo
            .list_recent_runs(50)
            .unwrap()
            .into_iter()
            .filter(|r| matches!(r.mode, crate::domain::agent::runtime::AgentRunMode::AccountTrigger))
            .count();
        assert_eq!(trig_runs, 2);
    }

    // ---- 账户自驱 quote tick（spec §6 行情/账户维护调度）----

    #[tokio::test]
    async fn account_tick_focused_refreshes_then_rebuilds_and_drains_eval_pages() {
        // subscribed=["600519.SH"]（CountingAccountGw）∪ core_indexes=["000300.SH"]。
        let (gw, rebuilds, evals) = CountingAccountGw::new(3); // 3 页 → 分页耗尽
        let refreshed = Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let quotes = Arc::new(TrackingQuotesGw {
            core: vec!["000300.SH".to_string()],
            refreshed: refreshed.clone(),
        });
        let (svc, _repo) =
            services_full_with_quotes(false, gw, Arc::new(StubGw), quotes);

        svc.run_account_eval_tick().await;

        // ① focused refresh 收到 subscribed ∪ core_indexes（去重后两只）。
        let calls = refreshed.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "focused refresh 应被调用一次");
        let codes = &calls[0];
        assert!(codes.contains(&"600519.SH".to_string()), "应含持仓/挂单标的");
        assert!(codes.contains(&"000300.SH".to_string()), "应含 core_indexes");
        // ② rebuild 一次 + ③ eval 分页耗尽（3 批）。
        assert_eq!(rebuilds.load(AtomicOrdering::SeqCst), 1, "应重建快照一次");
        assert_eq!(evals.load(AtomicOrdering::SeqCst), 3, "应分页耗尽 3 批");
    }

    #[tokio::test]
    async fn account_tick_empty_subscribed_skips_refresh_and_eval() {
        // StubGw account → subscribed_codes 默认空；StubGw quotes → core_indexes 默认空 → codes 空 → 跳过。
        let refreshed = Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let quotes = Arc::new(TrackingQuotesGw {
            core: vec![], // 空 core_indexes
            refreshed: refreshed.clone(),
        });
        let (svc, _repo) =
            services_full_with_quotes(false, Arc::new(StubGw), Arc::new(StubGw), quotes);

        svc.run_account_eval_tick().await;

        assert!(
            refreshed.lock().unwrap().is_empty(),
            "空 subscribed_codes ∪ core_indexes → 应跳过 focused refresh（零成本）"
        );
    }

    #[tokio::test]
    async fn account_tick_core_indexes_only_still_refreshes() {
        // 空仓无挂单无自选，但 core_indexes 非空 → 仍 focused refresh（基准估值）+ rebuild + eval。
        let (gw, rebuilds, evals) = CountingAccountGwNoSubs::new(1);
        let refreshed = Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let quotes = Arc::new(TrackingQuotesGw {
            core: vec!["000300.SH".to_string()],
            refreshed: refreshed.clone(),
        });
        let (svc, _repo) =
            services_full_with_quotes(false, gw, Arc::new(StubGw), quotes);

        svc.run_account_eval_tick().await;

        let calls = refreshed.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], vec!["000300.SH".to_string()]);
        assert_eq!(rebuilds.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(evals.load(AtomicOrdering::SeqCst), 1);
    }

    // ----------------------------------------------------- spec §9 对外接口 shape

    #[tokio::test]
    async fn dialogue_detailed_returns_message_id_and_run() {
        let (svc, repo) = services(true);
        let res = svc
            .run_dialogue_detailed("conv_x".into(), "看看茅台".into(), Vec::new())
            .await
            .unwrap();
        assert!(res.message_id.starts_with("msg_"), "messageId={}", res.message_id);
        // trigger.messageId == 返回的 messageId（同一锚）。
        if let AgentRunTrigger::UserChat { message_id } = &res.run.trigger {
            assert_eq!(message_id, &res.message_id);
        } else {
            panic!("dialogue run trigger must be UserChat");
        }
        let stored = repo.get_run(&res.run.run_id).unwrap().unwrap();
        assert_eq!(stored.status, AgentRunStatus::Completed);
    }

    #[tokio::test]
    async fn dialogue_detailed_accepts_images_without_panic() {
        // images 接受但当前不消费（text-only）；不应影响 run 完成。
        let (svc, _repo) = services(true);
        let res = svc
            .run_dialogue_detailed("conv_img".into(), "看图".into(), vec!["data:img".into()])
            .await
            .unwrap();
        assert_eq!(res.run.status, AgentRunStatus::Completed);
    }

    #[test]
    fn cancel_run_detailed_not_found() {
        let (svc, _repo) = services(false);
        let out = svc.cancel_run_detailed("nope");
        assert!(!out.accepted);
        assert_eq!(out.status, CancelRunStatus::NotFound);
    }

    #[tokio::test]
    async fn cancel_run_detailed_completed_run_not_acceptable() {
        let (svc, _repo) = services(true);
        // 跑完一个 dialogue run（终态 completed），其取消令牌已注销。
        let run = svc.run_dialogue("conv_c".into(), "hi".into()).await.unwrap();
        let out = svc.cancel_run_detailed(&run.run_id);
        assert!(!out.accepted, "completed run 不可撤");
        assert_eq!(out.status, CancelRunStatus::Completed);
    }

    #[tokio::test]
    async fn fetch_state_include_selector_and_offset() {
        let (svc, _repo) = services(true);
        // 跑两个 run 产生两条 runs。
        svc.run_dialogue("c1".into(), "a".into()).await.unwrap();
        svc.run_dialogue("c2".into(), "b".into()).await.unwrap();

        // 仅 runs：strategy/circuitBreaker 省略；trades 空。
        let only_runs = StateInclude {
            strategy: false,
            runs: true,
            analysis_results: false,
            trades: false,
            messages: false,
            tool_calls: false,
            circuit_breaker: false,
        };
        let snap = svc.fetch_state_with(only_runs, 50, 0).unwrap();
        assert!(snap.active_strategy.is_none(), "未选 strategy 应省略");
        assert!(snap.circuit_breaker_active.is_none(), "未选 circuitBreaker 应省略");
        assert_eq!(snap.recent_runs.len(), 2);
        assert!(snap.recent_results.is_empty());

        // limit=1 + offset=1 → 只回第二新的 run。
        let snap2 = svc.fetch_state_with(only_runs, 1, 1).unwrap();
        assert_eq!(snap2.recent_runs.len(), 1, "offset 分页生效");

        // 默认 include → strategy + runs + circuitBreaker 都在。
        let def = svc.fetch_state(50).unwrap();
        assert!(def.active_strategy.is_some());
        assert!(def.circuit_breaker_active.is_some());
        assert_eq!(def.recent_runs.len(), 2);
    }

    #[test]
    fn strategy_list_versions_for_include_history() {
        let (svc, _repo) = services(false);
        // baseline seeded as v1；再 upsert 一版 → v2。
        let (sid, v) = svc
            .strategy
            .upsert(None, Some(1), "收紧仓位".into(), crate::domain::agent::runtime::StrategyStatus::Active, "用户确认")
            .unwrap();
        assert_eq!(v, 2);
        let history = svc.strategy.list_versions(&sid).unwrap();
        assert_eq!(history.len(), 2, "应有 v1 + v2 两条历史");
        // 倒序：最新 v2 在前。
        assert_eq!(history[0].0, 2);
        assert_eq!(history[1].0, 1);
        assert_eq!(history[0].2.as_deref(), Some("用户确认"));
    }

    // ───── images 透传 ─────

    #[test]
    fn parse_data_url_image_valid() {
        use base64::Engine;
        let png = base64::engine::general_purpose::STANDARD.encode(b"fake-png-bytes");
        let url = format!("data:image/png;base64,{png}");
        let (mime, bytes) = super::parse_data_url_image(&url).expect("valid data-URL");
        assert_eq!(mime, "image/png");
        assert_eq!(bytes, b"fake-png-bytes");
    }

    #[test]
    fn parse_data_url_image_rejects_non_image() {
        assert!(super::parse_data_url_image("data:text/plain;base64,aGVsbG8=").is_none());
    }

    #[test]
    fn parse_data_url_image_rejects_garbage() {
        assert!(super::parse_data_url_image("not-a-data-url").is_none());
        assert!(super::parse_data_url_image("data:image/png,no-base64-marker").is_none());
    }

}
