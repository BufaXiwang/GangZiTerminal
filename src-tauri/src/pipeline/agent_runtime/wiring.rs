//! 工具注册装配 —— 按 mode 把领域 ToolSpec + 对应 handler 注册进一个 ToolRegistry。
//!
//! Spec: docs/design/agent-runtime-module.md §3 mode 表 / §4 工具
//!
//! 这是 executor 需要的 `registry` 的构造器：吃 gateway 句柄 + Runtime 服务，按 `domain_tools_for_mode`
//! 选定的工具集逐个注册 handler。**Infra 本地/skill/run_subagent 工具由 Infra 在同一 registry 上
//! 另行注册**（bootstrap 调 `register_local_tools` / `register_skill_tools`）；本函数只装领域工具。
//!
//! 临时复盘不另设 `run_review` 工具：对话/news 直接用 Infra `run_subagent`（allowedTools 收紧只读
//! `fetch_*`）拿回结论文本（spec §3/§4/§11）。

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::domain::agent::runtime::{AgentRunMode, AgentRunTrigger};
use crate::infrastructure::agent::messages_repo::AgentMessagesRepo;
use crate::infrastructure::agent::payload_store::PayloadStore;
use crate::infrastructure::agent::tool_registry::{ToolHandler, ToolRegistry};

use super::gateways::{AccountGateway, NewsGateway, QuotesGateway};
use super::handlers::{
    FetchAccountHandler, FetchNewsHandler, FetchQuotesHandler, OperateAccountHandler,
    RecordAnalysisHandler, RecordReviewSuggestionHandler, UpdateWatchlistHandler,
    UpsertStrategyHandler,
};
use super::records::RecordService;
use super::strategy::StrategyService;
use super::tools::{
    domain_tools_for_mode, FETCH_ACCOUNT, FETCH_NEWS, FETCH_QUOTES, OPERATE_ACCOUNT,
    RECORD_ANALYSIS, RECORD_REVIEW_SUGGESTION, UPDATE_WATCHLIST, UPSERT_INVESTMENT_STRATEGY,
};

/// 装配一次 run 用的领域工具依赖（gateway + Runtime 服务）。
#[derive(Clone)]
pub struct RuntimeToolDeps {
    pub quotes: Arc<dyn QuotesGateway>,
    pub news: Arc<dyn NewsGateway>,
    pub account: Arc<dyn AccountGateway>,
    pub strategy: Arc<StrategyService>,
    pub records: Arc<RecordService>,
    /// 持久化 tool_call 证据（run_id→ToolCall，spec §4「证据 = ToolCall 审计」）。
    /// `Some` → per-run registry 走持久化构造；`None` → 不持久化（单测）。
    pub persist: Option<(AgentMessagesRepo, PayloadStore)>,
    /// 编排级风控配置（阈值）。
    pub risk: super::risk::RiskConfig,
    /// 熔断标志（进程级共享）：operate_account handler 在自动 mode 下据此降级拒绝。
    pub circuit_breaker: Arc<AtomicBool>,
    /// 账户作用域 operate 串行锁（spec §4「按账户全局串行」）：本产品单一模拟账户 →
    /// 单个进程级全局锁；handler 在「记 submitting→operate→settle」临界区持锁，
    /// 保证并发 run 的 operate 不交错。
    pub operate_lock: Arc<tokio::sync::Mutex<()>>,
}

/// 按 mode 构造一个**只含领域工具**的 ToolRegistry（per-run：operate_account 绑定 run_id + 冻结策略版本）。
///
/// 返回的 registry 还需 bootstrap 注册 Infra 本地/skill 工具（同一 registry 实例上）。
pub fn build_domain_registry_for_mode(
    mode: AgentRunMode,
    trigger: &AgentRunTrigger,
    deps: &RuntimeToolDeps,
    run_id: &str,
    strategy_version: Option<u32>,
) -> Arc<ToolRegistry> {
    let registry = match &deps.persist {
        Some((repo, ps)) => ToolRegistry::new(repo.clone(), ps.clone()),
        None => ToolRegistry::new_without_persist(),
    };
    for spec in domain_tools_for_mode(mode) {
        let handler: Option<Arc<dyn ToolHandler>> = match spec.name.as_str() {
            FETCH_QUOTES => Some(Arc::new(FetchQuotesHandler::new(deps.quotes.clone()))),
            FETCH_NEWS => Some(Arc::new(FetchNewsHandler::new(deps.news.clone()))),
            FETCH_ACCOUNT => Some(Arc::new(FetchAccountHandler::new(deps.account.clone()))),
            UPDATE_WATCHLIST => Some(Arc::new(UpdateWatchlistHandler::new(deps.account.clone()))),
            OPERATE_ACCOUNT => Some(Arc::new(OperateAccountHandler::new(
                deps.account.clone(),
                deps.quotes.clone(),
                deps.records.clone(),
                deps.operate_lock.clone(),
                run_id,
                strategy_version,
                super::handlers::OperateGate {
                    circuit_breaker: deps.circuit_breaker.clone(),
                    // dialogue 在用户明确指令下仍可交易（spec §6）→ 不强制熔断/额度闸门；其余自动 mode 强制。
                    enforce_circuit_breaker: !matches!(mode, AgentRunMode::Dialogue),
                    // 当日额度（spec §6「自动 mode 不再开新仓」）：dialogue 不限，其余自动 mode 强制。
                    // cap 值由 handler 从账户 gateway `max_daily_new_orders()` 取，不在此硬编码。
                    enforce_daily_quota: !matches!(mode, AgentRunMode::Dialogue),
                    // 追高保护只对 news 触发的开仓生效（spec §6）。
                    enforce_chasing: matches!(mode, AgentRunMode::News),
                    chasing_guard_pct: deps.risk.chasing_guard_pct,
                },
            ))),
            // record_analysis 仅 news mode 暴露（domain_tools_for_mode 已保证），per-run 绑定 run_id。
            RECORD_ANALYSIS => Some(Arc::new(RecordAnalysisHandler::new(
                deps.records.clone(),
                run_id,
            ))),
            // record_review_suggestion 仅 review mode 暴露（domain_tools_for_mode 已保证），
            // per-run 绑定 run_id + 交易日（从 eod_review trigger 取，spec §3 ④）。
            RECORD_REVIEW_SUGGESTION => {
                if let AgentRunTrigger::EodReview { trade_date } = trigger {
                    Some(Arc::new(RecordReviewSuggestionHandler::new(
                        deps.records.clone(),
                        run_id,
                        trade_date.clone(),
                    )))
                } else {
                    // 防御：review tool 未挂 eod_review trigger（不该发生）→ 不注册（无 handler）。
                    None
                }
            }
            UPSERT_INVESTMENT_STRATEGY => {
                Some(Arc::new(UpsertStrategyHandler::new(deps.strategy.clone())))
            }
            _ => None,
        };
        if let Some(h) = handler {
            // 子 registry 是新空表，不会 Duplicate；忽略 Err 仅为稳健。
            let _ = registry.register_tool(spec, h);
        }
    }
    Arc::new(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::runtime::AccountResultRef;
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::agent::runtime_repo::AgentRuntimeRepo;
    use crate::infrastructure::db::{run_migrations, AppDb};
    use crate::pipeline::agent_runtime::gateways::{GatewayError, OperateOutcome};
    use async_trait::async_trait;
    use serde_json::{json, Value as JsonValue};

    struct StubGw;
    #[async_trait]
    impl QuotesGateway for StubGw {
        async fn fetch(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
    }
    #[async_trait]
    impl NewsGateway for StubGw {
        async fn fetch(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
    }
    #[async_trait]
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

    fn deps() -> RuntimeToolDeps {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        let repo = Arc::new(AgentRuntimeRepo::new(db));
        RuntimeToolDeps {
            quotes: Arc::new(StubGw),
            news: Arc::new(StubGw),
            account: Arc::new(StubGw),
            strategy: Arc::new(StrategyService::new(repo.clone())),
            records: Arc::new(RecordService::new(repo)),
            persist: None,
            risk: super::super::risk::RiskConfig::default(),
            circuit_breaker: Arc::new(AtomicBool::new(false)),
            operate_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// 该 mode 对应的一个代表性 trigger（registry 构造需要 trigger 取 review 的交易日）。
    fn trig(mode: AgentRunMode) -> AgentRunTrigger {
        match mode {
            AgentRunMode::Dialogue => AgentRunTrigger::UserChat { message_id: "m".into() },
            AgentRunMode::News => AgentRunTrigger::NewsBatch { news_ids: vec!["n".into()] },
            AgentRunMode::AccountTrigger => AgentRunTrigger::AccountTrigger { trigger_id: "t".into() },
            AgentRunMode::Review => AgentRunTrigger::EodReview {
                trade_date: crate::domain::shared::TradeDate::parse("20260605").unwrap(),
            },
        }
    }

    #[test]
    fn dialogue_registry_has_domain_tools_no_run_review() {
        let r = build_domain_registry_for_mode(
            AgentRunMode::Dialogue,
            &trig(AgentRunMode::Dialogue),
            &deps(),
            "run1",
            Some(1),
        );
        for t in [
            FETCH_QUOTES, FETCH_NEWS, FETCH_ACCOUNT, UPDATE_WATCHLIST, OPERATE_ACCOUNT,
            UPSERT_INVESTMENT_STRATEGY,
        ] {
            assert!(r.has_tool(t), "dialogue registry 缺 {t}");
        }
        // 不另设 run_review 工具：临时复盘用 Infra run_subagent（spec §3/§4/§11）。
        assert!(!r.has_tool("run_review"));
    }

    #[test]
    fn review_registry_is_readonly() {
        let r = build_domain_registry_for_mode(
            AgentRunMode::Review,
            &trig(AgentRunMode::Review),
            &deps(),
            "run1",
            None,
        );
        assert!(r.has_tool(FETCH_ACCOUNT) && r.has_tool(FETCH_QUOTES) && r.has_tool(FETCH_NEWS));
        assert!(!r.has_tool(OPERATE_ACCOUNT), "review registry 不得有 operate_account");
        assert!(!r.has_tool(UPSERT_INVESTMENT_STRATEGY));
        // review 暴露 record_review_suggestion（绑定 eod_review 交易日，spec §3 ④）。
        assert!(r.has_tool(RECORD_REVIEW_SUGGESTION), "review registry 缺 record_review_suggestion");
    }

    #[test]
    fn account_trigger_registry_can_operate_no_strategy() {
        let r = build_domain_registry_for_mode(
            AgentRunMode::AccountTrigger,
            &trig(AgentRunMode::AccountTrigger),
            &deps(),
            "run1",
            Some(1),
        );
        assert!(r.has_tool(OPERATE_ACCOUNT));
        assert!(!r.has_tool(UPSERT_INVESTMENT_STRATEGY));
        assert!(!r.has_tool(UPDATE_WATCHLIST));
        assert!(!r.has_tool(RECORD_ANALYSIS), "account_trigger 不该有 record_analysis");
    }

    #[test]
    fn record_analysis_registered_only_for_news() {
        let news = build_domain_registry_for_mode(
            AgentRunMode::News,
            &trig(AgentRunMode::News),
            &deps(),
            "run1",
            Some(1),
        );
        assert!(news.has_tool(RECORD_ANALYSIS), "news registry 缺 record_analysis");
        // 其它 mode registry 不得装 record_analysis（spec §3/§4/§6）。
        for m in [AgentRunMode::Dialogue, AgentRunMode::AccountTrigger, AgentRunMode::Review] {
            let r = build_domain_registry_for_mode(m, &trig(m), &deps(), "run1", Some(1));
            assert!(!r.has_tool(RECORD_ANALYSIS), "{m:?} 不该有 record_analysis");
        }
    }

    #[test]
    fn record_review_suggestion_registered_only_for_review() {
        let review = build_domain_registry_for_mode(
            AgentRunMode::Review,
            &trig(AgentRunMode::Review),
            &deps(),
            "run1",
            None,
        );
        assert!(review.has_tool(RECORD_REVIEW_SUGGESTION), "review registry 缺 record_review_suggestion");
        for m in [AgentRunMode::Dialogue, AgentRunMode::News, AgentRunMode::AccountTrigger] {
            let r = build_domain_registry_for_mode(m, &trig(m), &deps(), "run1", Some(1));
            assert!(!r.has_tool(RECORD_REVIEW_SUGGESTION), "{m:?} 不该有 record_review_suggestion");
        }
    }
}
