//! 领域 tool handler —— 把 `<use_tool>` 调用桥接到对应服务。
//!
//! Spec: docs/design/agent-runtime-module.md §4 工具
//!
//! 本文件目前实现 **Runtime 自有**服务的 handler（`upsert_investment_strategy` → `StrategyService`）。
//! 桥接到各 BC（`fetch_quotes`→Quotes / `operate_account`→Account 等）的 handler 在 bootstrap/adapter
//! 接线（WP4，那里握 BC service 句柄 + Runtime 单点生成 clientOrderId），遵循同一范式。

use std::sync::Arc;

use serde_json::json;

use crate::domain::agent::runtime::{AnalysisResultKind, StrategyStatus};
use crate::domain::shared::{ErrorCode, TsCode};
use crate::infrastructure::agent::tool_registry::{
    ToolHandler, ToolHandlerFuture, ToolHandlerOutput, ToolInvocation,
};

use super::gateways::{AccountGateway, NewsGateway, QuotesGateway};
use super::records::RecordService;
use super::strategy::{StrategyError, StrategyService};

/// `upsert_investment_strategy` handler —— 写策略新版本（仅 dialogue mode 暴露 + 用户确认后调用，
/// 由 mode 工具集 + 对话上层保证；handler 本身只执行 upsert）。
pub struct UpsertStrategyHandler {
    strategy: Arc<StrategyService>,
}

impl UpsertStrategyHandler {
    pub fn new(strategy: Arc<StrategyService>) -> Self {
        Self { strategy }
    }
}

impl ToolHandler for UpsertStrategyHandler {
    fn invoke(&self, inv: ToolInvocation) -> ToolHandlerFuture {
        let svc = self.strategy.clone();
        Box::pin(async move {
            let input = inv.input;
            let Some(strategy_text) = input.get("strategy").and_then(|v| v.as_str()) else {
                return ToolHandlerOutput::err(
                    json!({"accepted": false, "message": "字段 strategy 必填"}),
                    ErrorCode::InvalidInput,
                );
            };
            let reason = input.get("reason").and_then(|v| v.as_str()).unwrap_or("");
            let strategy_id = input.get("strategyId").and_then(|v| v.as_str());
            let base_version = input
                .get("baseVersion")
                .and_then(|v| v.as_u64())
                .map(|x| x as u32);
            let status = match input.get("status").and_then(|v| v.as_str()) {
                Some("paused") => StrategyStatus::Paused,
                _ => StrategyStatus::Active,
            };

            match svc.upsert(strategy_id, base_version, strategy_text.to_string(), status, reason) {
                Ok((id, version)) => ToolHandlerOutput::ok(
                    json!({"accepted": true, "strategyId": id, "version": version}),
                ),
                Err(StrategyError::VersionConflict { expected, actual }) => ToolHandlerOutput::err(
                    json!({
                        "accepted": false,
                        "reason": "version_conflict",
                        "expected": expected,
                        "actual": actual
                    }),
                    ErrorCode::VersionConflict,
                ),
                Err(StrategyError::Db(e)) => ToolHandlerOutput::err(
                    json!({"accepted": false, "message": e.to_string()}),
                    ErrorCode::DbError,
                ),
            }
        })
    }
}

// ───────────────────────── record_analysis（news 定论 → AnalysisResult）─────────

/// `record_analysis` handler —— **per-run**（绑定本次 run_id）。
///
/// 流程（spec §3 产生机制 / §4 handler）：解析 `{kind, summary, relatedCodes}` → `tradeIds` 由
/// Runtime 按 `run_id` 关联本 run 已记的 AgentTrades（`list_trades_by_run`，模型不提供）→
/// `record_analysis_result` 持久化 + emit `agent-analysis-result` → 回 `{resultId}`。
/// `kind` 非法 / `summary` 缺失 → `InvalidInput` 业务拒绝。仅 news mode 暴露。
pub struct RecordAnalysisHandler {
    records: Arc<RecordService>,
    run_id: String,
}
impl RecordAnalysisHandler {
    pub fn new(records: Arc<RecordService>, run_id: impl Into<String>) -> Self {
        Self {
            records,
            run_id: run_id.into(),
        }
    }
}
impl ToolHandler for RecordAnalysisHandler {
    fn invoke(&self, inv: ToolInvocation) -> ToolHandlerFuture {
        let rec = self.records.clone();
        let run_id = self.run_id.clone();
        Box::pin(async move {
            let input = inv.input;
            // kind：必填，封闭集 action|no_action。
            let kind = match input.get("kind").and_then(|v| v.as_str()) {
                Some("action") => AnalysisResultKind::Action,
                Some("no_action") => AnalysisResultKind::NoAction,
                _ => {
                    return ToolHandlerOutput::err(
                        json!({"message": "字段 kind 必填且须为 action|no_action"}),
                        ErrorCode::InvalidInput,
                    )
                }
            };
            // summary：必填非空。
            let Some(summary) = input.get("summary").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) else {
                return ToolHandlerOutput::err(
                    json!({"message": "字段 summary 必填"}),
                    ErrorCode::InvalidInput,
                );
            };
            // relatedCodes：可空；非法 tsCode 跳过（不阻断定论）。
            let related_codes: Vec<TsCode> = input
                .get("relatedCodes")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|c| c.as_str())
                        .filter_map(|s| TsCode::parse(s).ok())
                        .collect()
                })
                .unwrap_or_default();

            // tradeIds：Runtime 按 run_id 关联本 run 已记的 AgentTrades（模型不提供）。
            let trade_ids = rec
                .list_trades_by_run(&run_id)
                .map(|ts| ts.into_iter().map(|t| t.trade_id).collect::<Vec<_>>())
                .unwrap_or_default();

            match rec.record_analysis_result(&run_id, kind, summary, related_codes, trade_ids) {
                Ok(r) => ToolHandlerOutput::ok(json!({"resultId": r.result_id})),
                Err(e) => ToolHandlerOutput::err(
                    json!({"message": e.to_string()}),
                    ErrorCode::DbError,
                ),
            }
        })
    }
}

// ───────────────────── record_review_suggestion（复盘建议 → ReviewSuggestion）────

/// `record_review_suggestion` handler —— **per-run**（绑定本次 review run_id + 交易日）。
///
/// 流程（spec §3 ④ follow-up）：解析 `{text}` → 经 `record_review_suggestion` 持久化为
/// `ReviewSuggestion{suggestionId, reviewRunId, tradeDate, text}` → 回 `{suggestionId}`。
/// 仅 review mode 暴露。`text` 缺失/空 → `InvalidInput`。**只登记建议，不改策略。**
pub struct RecordReviewSuggestionHandler {
    records: Arc<RecordService>,
    run_id: String,
    trade_date: crate::domain::shared::TradeDate,
}
impl RecordReviewSuggestionHandler {
    pub fn new(
        records: Arc<RecordService>,
        run_id: impl Into<String>,
        trade_date: crate::domain::shared::TradeDate,
    ) -> Self {
        Self {
            records,
            run_id: run_id.into(),
            trade_date,
        }
    }
}
impl ToolHandler for RecordReviewSuggestionHandler {
    fn invoke(&self, inv: ToolInvocation) -> ToolHandlerFuture {
        let rec = self.records.clone();
        let run_id = self.run_id.clone();
        let trade_date = self.trade_date.clone();
        Box::pin(async move {
            let Some(text) = inv
                .input
                .get("text")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
            else {
                return ToolHandlerOutput::err(
                    json!({"message": "字段 text 必填（非空策略建议文本）"}),
                    ErrorCode::InvalidInput,
                );
            };
            match rec.record_review_suggestion(&run_id, trade_date, text) {
                Ok(s) => ToolHandlerOutput::ok(json!({"suggestionId": s.suggestion_id})),
                Err(e) => {
                    ToolHandlerOutput::err(json!({"message": e.to_string()}), ErrorCode::DbError)
                }
            }
        })
    }
}

// ───────────────────────── 只读 BC handler（fetch_*）─────────────────────────

/// `fetch_quotes` → QuotesGateway。
pub struct FetchQuotesHandler {
    gateway: Arc<dyn QuotesGateway>,
}
impl FetchQuotesHandler {
    pub fn new(gateway: Arc<dyn QuotesGateway>) -> Self {
        Self { gateway }
    }
}
impl ToolHandler for FetchQuotesHandler {
    fn invoke(&self, inv: ToolInvocation) -> ToolHandlerFuture {
        let gw = self.gateway.clone();
        Box::pin(async move {
            match gw.fetch(inv.input).await {
                Ok(v) => ToolHandlerOutput::ok(v),
                Err(e) => ToolHandlerOutput::err(json!({"message": e.message}), e.code),
            }
        })
    }
}

/// `fetch_news` → NewsGateway。
pub struct FetchNewsHandler {
    gateway: Arc<dyn NewsGateway>,
}
impl FetchNewsHandler {
    pub fn new(gateway: Arc<dyn NewsGateway>) -> Self {
        Self { gateway }
    }
}
impl ToolHandler for FetchNewsHandler {
    fn invoke(&self, inv: ToolInvocation) -> ToolHandlerFuture {
        let gw = self.gateway.clone();
        Box::pin(async move {
            match gw.fetch(inv.input).await {
                Ok(v) => ToolHandlerOutput::ok(v),
                Err(e) => ToolHandlerOutput::err(json!({"message": e.message}), e.code),
            }
        })
    }
}

/// `fetch_account` → AccountGateway（只读）。
pub struct FetchAccountHandler {
    gateway: Arc<dyn AccountGateway>,
}
impl FetchAccountHandler {
    pub fn new(gateway: Arc<dyn AccountGateway>) -> Self {
        Self { gateway }
    }
}
impl ToolHandler for FetchAccountHandler {
    fn invoke(&self, inv: ToolInvocation) -> ToolHandlerFuture {
        let gw = self.gateway.clone();
        Box::pin(async move {
            match gw.fetch(inv.input).await {
                Ok(v) => ToolHandlerOutput::ok(v),
                Err(e) => ToolHandlerOutput::err(json!({"message": e.message}), e.code),
            }
        })
    }
}

/// `update_watchlist` → AccountGateway。
pub struct UpdateWatchlistHandler {
    gateway: Arc<dyn AccountGateway>,
}
impl UpdateWatchlistHandler {
    pub fn new(gateway: Arc<dyn AccountGateway>) -> Self {
        Self { gateway }
    }
}
impl ToolHandler for UpdateWatchlistHandler {
    fn invoke(&self, inv: ToolInvocation) -> ToolHandlerFuture {
        let gw = self.gateway.clone();
        Box::pin(async move {
            match gw.update_watchlist(inv.input).await {
                Ok(v) => ToolHandlerOutput::ok(v),
                Err(e) => ToolHandlerOutput::err(json!({"message": e.message}), e.code),
            }
        })
    }
}

// ───────────────────────── operate_account（核心交易写桥）─────────────────────

/// `operate_account` → AccountGateway，**per-run** handler（绑定本次 run_id + 冻结策略版本）。
///
/// 流程（spec §4 handler）：生成 `client_order_id` 注入 → 落 `AgentTrade{submitting}` →
/// 调 Account（gateway，Account 按 clientOrderId 去重 + fail-closed 兜底）→ 拿结果 `settle`（accepted
/// 且有 orderId → 写 orderId→runId 索引）→ 回结构化结果。被拒是**业务结果**（accepted=false），
/// 不是 tool error。风控全部交给 AI Agent 策略层自行管理。
pub struct OperateAccountHandler {
    gateway: Arc<dyn AccountGateway>,
    records: Arc<RecordService>,
    /// 账户作用域串行锁（spec §4「按账户全局串行」）：临界区 = 记 submitting→operate→settle。
    operate_lock: Arc<tokio::sync::Mutex<()>>,
    run_id: String,
    strategy_version: Option<u32>,
}
impl OperateAccountHandler {
    pub fn new(
        gateway: Arc<dyn AccountGateway>,
        records: Arc<RecordService>,
        operate_lock: Arc<tokio::sync::Mutex<()>>,
        run_id: impl Into<String>,
        strategy_version: Option<u32>,
    ) -> Self {
        Self {
            gateway,
            records,
            operate_lock,
            run_id: run_id.into(),
            strategy_version,
        }
    }
}
impl ToolHandler for OperateAccountHandler {
    fn invoke(&self, inv: ToolInvocation) -> ToolHandlerFuture {
        let gw = self.gateway.clone();
        let rec = self.records.clone();
        let operate_lock = self.operate_lock.clone();
        let run_id = self.run_id.clone();
        let sv = self.strategy_version;
        Box::pin(async move {
            let input = inv.input;
            let Some(account_input) = input.get("accountInput").cloned() else {
                return ToolHandlerOutput::err(
                    json!({"accepted": false, "message": "字段 accountInput 必填"}),
                    ErrorCode::InvalidInput,
                );
            };
            let reason = input.get("reason").and_then(|v| v.as_str()).unwrap_or("");

            let summary = {
                let action = account_input
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("operate");
                let code = account_input.get("tsCode").and_then(|v| v.as_str()).unwrap_or("");
                // 开仓动作打 `[open]` 标记 → 当日额度统计口径可据此只数新开仓（spec §6）。
                let prefix = if is_open_action(&account_input) { OPEN_SUMMARY_MARKER } else { "" };
                if code.is_empty() {
                    format!("{prefix}{action}")
                } else {
                    format!("{prefix}{action} {code}")
                }
            };

            // 账户作用域串行（spec §4「按账户全局串行」）：从此持锁，保证同一时刻只有一个
            // operate 在跑「记 submitting→operate→settle」，避免并发 run 交错产生 race。
            let _guard = operate_lock.lock().await;

            // 1) Runtime 单点生成 clientOrderId → 落 submitting。
            let client_order_id = RecordService::new_client_order_id();
            let trade = match rec.record_trade_submitting(&run_id, &client_order_id, sv, reason, summary) {
                Ok(t) => t,
                Err(e) => {
                    return ToolHandlerOutput::err(
                        json!({"accepted": false, "message": e.to_string()}),
                        ErrorCode::DbError,
                    )
                }
            };

            // 2) 调 Account（透传 clientOrderId）。
            let outcome = gw.operate(account_input, &client_order_id, reason).await;
            let result = outcome.result; // 部分移动：outcome.snapshot/warnings 仍可访问。

            // 3) settle（accepted + orderId → 写 orderId→runId 索引）。
            if let Err(e) = rec.settle_trade(&trade, result.clone()) {
                return ToolHandlerOutput::err(
                    json!({"accepted": false, "tradeId": trade.trade_id, "message": e.to_string()}),
                    ErrorCode::DbError,
                );
            }

            // 4) 业务结果（被拒不是 tool error）。snapshot/warnings 回模型（spec §4 OperateAccountToolOutput），不入 AgentTrade。
            ToolHandlerOutput::ok(json!({
                "accepted": result.accepted,
                "tradeId": trade.trade_id,
                "orderId": result.order_id,
                "fillIds": result.fill_ids,
                "positionId": result.position_id,
                "accountEventIds": result.account_event_ids,
                "rejectionEventId": result.rejection_event_id,
                "reason": result.reason,
                "message": result.message,
                "snapshot": outcome.snapshot,
                "warnings": outcome.warnings,
            }))
        })
    }
}

/// AgentTrade 摘要中「新开仓」标记前缀（当日意图摘要统计用）。
const OPEN_SUMMARY_MARKER: &str = "[open] ";

/// accountInput 是否「新开仓」动作（spec §6 当日额度只拦开仓）：`open_position` / `place_order side=buy`。
/// 平仓（close_position）/ 调仓（scale_position）/ 撤单（cancel_order）/ 调整保护（adjust_protection）
/// 等不计——保证「达上限后仍可平/调仓」。
fn is_open_action(input: &serde_json::Value) -> bool {
    match input.get("action").and_then(|v| v.as_str()) {
        Some("open_position") => true,
        Some("place_order") => input.get("side").and_then(|v| v.as_str()) == Some("buy"),
        _ => false,
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::agent::runtime_repo::AgentRuntimeRepo;
    use crate::infrastructure::db::{run_migrations, AppDb};

    fn test_lock() -> Arc<tokio::sync::Mutex<()>> {
        Arc::new(tokio::sync::Mutex::new(()))
    }

    fn handler_with_seeded() -> (UpsertStrategyHandler, Arc<StrategyService>) {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        let svc = Arc::new(StrategyService::new(Arc::new(AgentRuntimeRepo::new(db))));
        svc.seed_baseline_if_empty().unwrap(); // active v1
        (UpsertStrategyHandler::new(svc.clone()), svc)
    }

    fn inv(input: serde_json::Value) -> ToolInvocation {
        ToolInvocation {
            run_id: "run1".into(),
            tool_call_id: "tc1".into(),
            name: "upsert_investment_strategy".into(),
            input,
        }
    }

    #[tokio::test]
    async fn upsert_bumps_version_via_handler() {
        let (h, svc) = handler_with_seeded();
        let out = h
            .invoke(inv(json!({"strategy": "更保守：单票不超 20%。", "reason": "用户确认"})))
            .await;
        assert!(!out.is_error);
        assert_eq!(out.output_summary["accepted"], true);
        assert_eq!(out.output_summary["version"], 2);
        assert_eq!(svc.active().unwrap().unwrap().version, 2);
    }

    #[tokio::test]
    async fn missing_strategy_is_invalid_input() {
        let (h, _svc) = handler_with_seeded();
        let out = h.invoke(inv(json!({"reason": "x"}))).await;
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::InvalidInput));
    }

    #[tokio::test]
    async fn base_version_conflict_reported() {
        let (h, _svc) = handler_with_seeded();
        let out = h
            .invoke(inv(json!({"strategy": "x", "reason": "r", "baseVersion": 99})))
            .await;
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::VersionConflict));
        assert_eq!(out.output_summary["reason"], "version_conflict");
        assert_eq!(out.output_summary["actual"], 1);
    }

    // ---- operate_account handler（mock AccountGateway）----

    use crate::domain::agent::channel::WireFormat;
    use crate::domain::agent::runtime::{AccountResultRef, AgentRun, AgentRunMode, AgentRunStatus, AgentRunTrigger};
    use crate::pipeline::agent_runtime::gateways::{GatewayError, OperateOutcome};
    use chrono::Utc;
    use serde_json::Value as JsonValue;

    struct MockAccount {
        result: AccountResultRef,
        seen_client_order_id: std::sync::Mutex<Option<String>>,
    }
    impl MockAccount {
        fn new(result: AccountResultRef) -> Self {
            Self {
                result,
                seen_client_order_id: std::sync::Mutex::new(None),
            }
        }
    }
    #[async_trait::async_trait]
    impl AccountGateway for MockAccount {
        async fn fetch(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({}))
        }
        async fn operate(
            &self,
            _account_input: JsonValue,
            client_order_id: &str,
            _reason: &str,
        ) -> OperateOutcome {
            *self.seen_client_order_id.lock().unwrap() = Some(client_order_id.to_string());
            OperateOutcome::from_result(self.result.clone())
        }
        async fn update_watchlist(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({"ok": true}))
        }
    }

    fn records_with_run() -> (Arc<RecordService>, Arc<AgentRuntimeRepo>) {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        let repo = Arc::new(AgentRuntimeRepo::new(db));
        repo.insert_run(
            &AgentRun {
                run_id: "run1".into(),
                mode: AgentRunMode::News,
                trigger: AgentRunTrigger::NewsBatch { news_ids: vec![] },
                parent_run_id: None,
                provider: "p".into(),
                wire_format: WireFormat::Messages,
                model: "m".into(),
                strategy_version: Some(1),
                causation_run_id: None,
                status: AgentRunStatus::Running,
                started_at: Some(Utc::now()),
                ended_at: None,
                error: None,
            },
            Utc::now(),
        )
        .unwrap();
        (Arc::new(RecordService::new(repo.clone())), repo)
    }

    fn op_inv(account_input: JsonValue) -> ToolInvocation {
        ToolInvocation {
            run_id: "run1".into(),
            tool_call_id: "tc1".into(),
            name: "operate_account".into(),
            input: json!({"accountInput": account_input, "reason": "news 利好开仓"}),
        }
    }

    #[tokio::test]
    async fn operate_accepted_records_submitting_then_settled_and_order_index() {
        let (records, repo) = records_with_run();
        let gw = Arc::new(MockAccount::new(AccountResultRef {
            accepted: true,
            order_id: Some("ord1".into()),
            fill_ids: vec!["f1".into()],
            position_id: Some("pos1".into()),
            account_event_ids: vec!["e1".into()],
            rejection_event_id: None,
            reason: None,
            message: None,
        }));
        let h = OperateAccountHandler::new(gw.clone(), records, test_lock(), "run1", Some(1));
        let out = h
            .invoke(op_inv(json!({"action": "open_position", "tsCode": "600519.SH", "quantity": 100})))
            .await;
        assert!(!out.is_error);
        assert_eq!(out.output_summary["accepted"], true);
        assert_eq!(out.output_summary["orderId"], "ord1");
        // Runtime 单点生成了 clientOrderId 并透传给 Account。
        assert!(gw.seen_client_order_id.lock().unwrap().as_ref().unwrap().starts_with("co_"));
        // submitting→settled：无悬挂；accepted+orderId → 写了 orderId→runId 索引。
        assert_eq!(repo.list_submitting_trades().unwrap().len(), 0);
        assert!(repo.find_run_by_order_id("ord1").unwrap().is_some());
    }

    #[tokio::test]
    async fn operate_rejected_is_business_result_not_tool_error() {
        let (records, repo) = records_with_run();
        let gw = Arc::new(MockAccount::new(AccountResultRef {
            accepted: false,
            order_id: None,
            fill_ids: vec![],
            position_id: None,
            account_event_ids: vec![],
            rejection_event_id: Some("rej1".into()),
            reason: Some(ErrorCode::QuoteStale),
            message: Some("行情过期".into()),
        }));
        let h = OperateAccountHandler::new(gw, records, test_lock(), "run1", Some(1));
        let out = h
            .invoke(op_inv(json!({"action": "open_position", "tsCode": "600519.SH", "quantity": 100})))
            .await;
        // 拒单是业务结果，不是 tool error。
        assert!(!out.is_error);
        assert_eq!(out.output_summary["accepted"], false);
        // 失败也记 settled（无悬挂），但不写 order 索引。
        assert_eq!(repo.list_submitting_trades().unwrap().len(), 0);
        assert!(repo.find_run_by_order_id("ord1").unwrap().is_none());
    }

    // ---- record_analysis handler ----

    use crate::domain::agent::runtime::AnalysisResultKind;
    use crate::pipeline::agent_runtime::runs::{RuntimeEvent, RuntimeEventSink};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ra_inv(input: JsonValue) -> ToolInvocation {
        ToolInvocation {
            run_id: "run1".into(),
            tool_call_id: "tc1".into(),
            name: "record_analysis".into(),
            input,
        }
    }

    #[tokio::test]
    async fn record_analysis_persists_and_emits_no_action() {
        let (records, repo) = records_with_run();
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let sink: RuntimeEventSink = Arc::new(move |ev| {
            if let RuntimeEvent::AnalysisResultEmitted { kind, .. } = ev {
                assert_eq!(kind, AnalysisResultKind::NoAction);
                n2.fetch_add(1, Ordering::SeqCst);
            }
        });
        records.set_event_sink(sink);
        let h = RecordAnalysisHandler::new(records, "run1");
        let out = h
            .invoke(ra_inv(json!({
                "kind": "no_action",
                "summary": "利好已 price-in，观望",
                "relatedCodes": ["600519.SH"]
            })))
            .await;
        assert!(!out.is_error);
        assert!(out.output_summary["resultId"].as_str().unwrap().starts_with("ar_"));
        // 持久化可读回。
        let results = repo.list_analysis_results_by_run("run1").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].kind, AnalysisResultKind::NoAction);
        // emit 了 agent-analysis-result。
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn record_analysis_action_links_run_trades() {
        let (records, repo) = records_with_run();
        // 本 run 先有一笔已结算 AgentTrade。
        let coid = RecordService::new_client_order_id();
        let t = records
            .record_trade_submitting("run1", coid.as_str(), Some(1), "开仓", "[open] open_position 600519.SH")
            .unwrap();
        records.settle_trade(&t, AccountResultRef { accepted: true, order_id: Some("ord1".into()), ..Default::default() }).unwrap();

        let h = RecordAnalysisHandler::new(records, "run1");
        let out = h
            .invoke(ra_inv(json!({"kind": "action", "summary": "边际信息强，开仓", "relatedCodes": []})))
            .await;
        assert!(!out.is_error);
        let results = repo.list_analysis_results_by_run("run1").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].kind, AnalysisResultKind::Action);
        // tradeIds 由 Runtime 按 run_id 关联上本 run 的 AgentTrade。
        assert_eq!(results[0].trade_ids, vec![t.trade_id]);
    }

    #[tokio::test]
    async fn record_analysis_invalid_kind_is_invalid_input() {
        let (records, _repo) = records_with_run();
        let h = RecordAnalysisHandler::new(records, "run1");
        let out = h.invoke(ra_inv(json!({"kind": "buy", "summary": "x"}))).await;
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::InvalidInput));
    }

    #[tokio::test]
    async fn record_analysis_missing_summary_is_invalid_input() {
        let (records, _repo) = records_with_run();
        let h = RecordAnalysisHandler::new(records, "run1");
        let out = h.invoke(ra_inv(json!({"kind": "no_action"}))).await;
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::InvalidInput));
    }

    #[tokio::test]
    async fn operate_missing_account_input_is_invalid() {
        let (records, _repo) = records_with_run();
        let gw = Arc::new(MockAccount::new(AccountResultRef::default()));
        let h = OperateAccountHandler::new(gw, records, test_lock(), "run1", Some(1));
        let out = h
            .invoke(ToolInvocation {
                run_id: "run1".into(),
                tool_call_id: "tc1".into(),
                name: "operate_account".into(),
                input: json!({"reason": "x"}),
            })
            .await;
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::InvalidInput));
    }
}
