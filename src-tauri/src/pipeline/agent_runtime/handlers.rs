//! 领域 tool handler —— 把 `<use_tool>` 调用桥接到对应服务。
//!
//! Spec: docs/design/agent-runtime-module.md §4 工具
//!
//! 本文件目前实现 **Runtime 自有**服务的 handler（`upsert_investment_strategy` → `StrategyService`）。
//! 桥接到各 BC（`fetch_quotes`→Quotes / `operate_account`→Account 等）的 handler 在 bootstrap/adapter
//! 接线（WP4，那里握 BC service 句柄 + Runtime 单点生成 clientOrderId），遵循同一范式。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{DateTime, FixedOffset, TimeZone, Utc};
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
/// 不是 tool error（spec：拒绝型业务结果不一定 isError）。
///
/// operate_account 的编排级闸门配置（spec §6，按 mode 装配）。
#[derive(Clone)]
pub struct OperateGate {
    /// 熔断标志（Runtime 进程级共享；set_circuit_breaker / 自动熔断翻转）。
    pub circuit_breaker: Arc<AtomicBool>,
    /// 熔断闸门是否对本 run 生效（dialogue=false：用户明确指令下仍可交易，spec §6）。
    pub enforce_circuit_breaker: bool,
    /// 当日额度闸门是否对本 run 生效（dialogue=false：用户明确指令下不受额度限制，spec §6
    /// 「自动 mode 不再开新仓」）。
    pub enforce_daily_quota: bool,
    /// 追高保护是否对本 run 生效（news 触发的开仓=true）。
    pub enforce_chasing: bool,
    /// 追高阈值（当日涨幅分数，0.05 = 5%）。
    pub chasing_guard_pct: f64,
}

/// `operate_account` → AccountGateway，**per-run** handler（绑定本次 run_id + 冻结策略版本）。
///
/// 编排级风控闸门（spec §4 handler ① / §6）：① 熔断激活（自动 mode）→ 降级拒绝；② 当日额度耗尽 →
/// 拒绝；③ 追高保护（news 开仓且标的当日涨幅超阈值/临近涨停）→ 降级拒绝。三者都是**业务拒绝**
/// （accepted=false, blocked=true），不是 tool error，且都拦在记账（submitting）之前。
pub struct OperateAccountHandler {
    gateway: Arc<dyn AccountGateway>,
    quotes: Arc<dyn QuotesGateway>,
    records: Arc<RecordService>,
    /// 账户作用域串行锁（spec §4「按账户全局串行」）：临界区 = 记 submitting→operate→settle。
    operate_lock: Arc<tokio::sync::Mutex<()>>,
    run_id: String,
    strategy_version: Option<u32>,
    gate: OperateGate,
}
impl OperateAccountHandler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        gateway: Arc<dyn AccountGateway>,
        quotes: Arc<dyn QuotesGateway>,
        records: Arc<RecordService>,
        operate_lock: Arc<tokio::sync::Mutex<()>>,
        run_id: impl Into<String>,
        strategy_version: Option<u32>,
        gate: OperateGate,
    ) -> Self {
        Self {
            gateway,
            quotes,
            records,
            operate_lock,
            run_id: run_id.into(),
            strategy_version,
            gate,
        }
    }
}
impl ToolHandler for OperateAccountHandler {
    fn invoke(&self, inv: ToolInvocation) -> ToolHandlerFuture {
        let gw = self.gateway.clone();
        let quotes = self.quotes.clone();
        let rec = self.records.clone();
        let operate_lock = self.operate_lock.clone();
        let run_id = self.run_id.clone();
        let sv = self.strategy_version;
        let gate = self.gate.clone();
        Box::pin(async move {
            let input = inv.input;
            let Some(account_input) = input.get("accountInput").cloned() else {
                return ToolHandlerOutput::err(
                    json!({"accepted": false, "message": "字段 accountInput 必填"}),
                    ErrorCode::InvalidInput,
                );
            };
            let reason = input.get("reason").and_then(|v| v.as_str()).unwrap_or("");

            // ① 熔断闸门（spec §4 handler ①）：熔断激活 → 自动 mode 降级拒绝（业务结果，非 tool error）。
            if gate.enforce_circuit_breaker && gate.circuit_breaker.load(Ordering::Relaxed) {
                return ToolHandlerOutput::ok(json!({
                    "accepted": false,
                    "blocked": true,
                    "message": "熔断激活：自动下单已降级为 no_action/建议，需用户在对话中确认解除（set_circuit_breaker）。",
                }));
            }
            // ② 当日额度（spec §6）：cap = 账户级 `maxDailyNewOrders`（gateway 取，不硬编码）；
            //    统计口径**只数当日新开仓**——平仓/调仓不计；闸门也只拦「开仓」动作（平仓/调仓放行）。
            //    dialogue（enforce_daily_quota=false）用户明确指令下不受额度限制。
            if gate.enforce_daily_quota && is_open_action(&account_input) {
                let cap = gw.max_daily_new_orders();
                let today_opens = rec
                    .list_trades_since(china_day_start(Utc::now()))
                    .map(|v| v.iter().filter(|t| is_open_summary(&t.account_input_summary)).count() as u32)
                    .unwrap_or(0);
                if today_opens >= cap {
                    return ToolHandlerOutput::ok(json!({
                        "accepted": false,
                        "blocked": true,
                        "message": format!("当日新开仓额度耗尽（{today_opens}/{cap}），自动 mode 不再开新仓（可平仓/调仓）。"),
                    }));
                }
            }
            // ③ 追高保护（spec §6）：news 开仓且标的当日涨幅超阈值/临近涨停 → 降级。
            if gate.enforce_chasing {
                if let Some(ts) = open_buy_ts(&account_input) {
                    if let Ok(q) = quotes
                        .fetch(json!({"tsCodes": [ts], "include": {"quote": true}}))
                        .await
                    {
                        if let Some((change_frac, near_limit)) = extract_chasing(&q) {
                            let cfg = super::risk::RiskConfig {
                                chasing_guard_pct: gate.chasing_guard_pct,
                                ..Default::default()
                            };
                            if super::risk::chasing_guard_triggered(change_frac, near_limit, &cfg) {
                                return ToolHandlerOutput::ok(json!({
                                    "accepted": false,
                                    "blocked": true,
                                    "message": "追高保护：标的当日涨幅过高/临近涨停，开仓降级为 no_action/仅加自选；要追须用户在对话中确认。",
                                }));
                            }
                        }
                    }
                }
            }
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
            // 风控闸门（①②③）已在锁外完成（只读 / 业务降级，不写账户）。
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

/// 若 accountInput 是「开仓买入」（open_position / place_order side=buy）→ 返回标的 tsCode。
/// scale_position 用 positionId 无 tsCode，不参与追高判定（spec §6 仅约束「开仓」）。
fn open_buy_ts(input: &serde_json::Value) -> Option<String> {
    match input.get("action").and_then(|v| v.as_str())? {
        "open_position" => input.get("tsCode").and_then(|v| v.as_str()).map(String::from),
        "place_order" if input.get("side").and_then(|v| v.as_str()) == Some("buy") => {
            input.get("tsCode").and_then(|v| v.as_str()).map(String::from)
        }
        _ => None,
    }
}

/// AgentTrade 摘要中「新开仓」标记前缀（当日额度只数开仓，spec §6）。
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

/// 历史 AgentTrade 是否为「新开仓」—— 据记账时打的 `[open]` 标记（spec §6 当日额度统计口径）。
fn is_open_summary(summary: &str) -> bool {
    summary.starts_with(OPEN_SUMMARY_MARKER)
}

/// 从 fetch_quotes 结果取 (今日涨幅分数, 是否临近涨停)。changePercent 是百分数（5.0=5%）→ /100。
/// 临近涨停：(limitUp − price)/limitUp ≤ 0.5%。字段缺失则相应项退化（涨幅 0 / 不临近）。
fn extract_chasing(q: &serde_json::Value) -> Option<(f64, bool)> {
    let quote = q.get("items")?.as_array()?.first()?.get("quote")?;
    let change_frac = quote
        .get("changePercent")
        .and_then(|v| v.as_f64())
        .map(|p| p / 100.0)
        .unwrap_or(0.0);
    let price = quote.get("price").and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok());
    let limit_up = quote.get("limitUp").and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok());
    let near_limit = match (price, limit_up) {
        (Some(p), Some(lu)) if lu > 0.0 => (lu - p) / lu <= 0.005,
        _ => false,
    };
    Some((change_frac, near_limit))
}

/// 当日 0 点（Asia/Shanghai，UTC+8）对应的 UTC 瞬间 —— A 股交易日边界（当日额度统计起点）。
fn china_day_start(now: DateTime<Utc>) -> DateTime<Utc> {
    let cn = FixedOffset::east_opt(8 * 3600).expect("valid offset");
    let local = now.with_timezone(&cn);
    let start = local
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight valid");
    cn.from_local_datetime(&start)
        .single()
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::agent::runtime_repo::AgentRuntimeRepo;
    use crate::infrastructure::db::{run_migrations, AppDb};

    /// mock 行情：可配涨幅/价/涨停价（追高测试用）。默认平静（涨幅 0、远离涨停）。
    struct MockQuotes {
        change_percent: f64,
        price: f64,
        limit_up: f64,
    }
    #[async_trait::async_trait]
    impl QuotesGateway for MockQuotes {
        async fn fetch(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, super::super::gateways::GatewayError> {
            Ok(json!({"items": [{"quote": {
                "changePercent": self.change_percent,
                "price": self.price.to_string(),
                "limitUp": self.limit_up.to_string(),
            }}]}))
        }
    }
    fn quiet_quotes() -> Arc<dyn QuotesGateway> {
        Arc::new(MockQuotes { change_percent: 0.0, price: 10.0, limit_up: 11.0 })
    }
    fn test_lock() -> Arc<tokio::sync::Mutex<()>> {
        Arc::new(tokio::sync::Mutex::new(()))
    }
    fn gate(
        cb: Arc<std::sync::atomic::AtomicBool>,
        enforce_cb: bool,
        enforce_quota: bool,
        enforce_chasing: bool,
    ) -> OperateGate {
        OperateGate {
            circuit_breaker: cb,
            enforce_circuit_breaker: enforce_cb,
            enforce_daily_quota: enforce_quota,
            enforce_chasing,
            chasing_guard_pct: 0.05,
        }
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
        /// 账户级当日新开仓上限（gateway `max_daily_new_orders()` 来源；缺省 u32::MAX = 不限）。
        max_daily_new_orders: u32,
    }
    impl MockAccount {
        fn new(result: AccountResultRef) -> Self {
            Self {
                result,
                seen_client_order_id: std::sync::Mutex::new(None),
                max_daily_new_orders: u32::MAX,
            }
        }
        fn with_cap(result: AccountResultRef, cap: u32) -> Self {
            Self {
                result,
                seen_client_order_id: std::sync::Mutex::new(None),
                max_daily_new_orders: cap,
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
        fn max_daily_new_orders(&self) -> u32 {
            self.max_daily_new_orders
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
        let h = OperateAccountHandler::new(gw.clone(), quiet_quotes(), records, test_lock(), "run1", Some(1), gate(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)), true, false, false));
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
        let h = OperateAccountHandler::new(gw, quiet_quotes(), records, test_lock(), "run1", Some(1), gate(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)), true, false, false));
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

    #[tokio::test]
    async fn circuit_breaker_blocks_auto_mode_without_recording_trade() {
        let (records, repo) = records_with_run();
        let gw = Arc::new(MockAccount::new(AccountResultRef { accepted: true, ..Default::default() }));
        let cb = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)); // 熔断激活
        // enforce=true（news/account_trigger 等自动 mode）→ 应降级拒绝。
        let h = OperateAccountHandler::new(gw.clone(), quiet_quotes(), records, test_lock(), "run1", Some(1), gate(cb, true, false, false));
        let out = h
            .invoke(op_inv(json!({"action": "open_position", "tsCode": "600519.SH", "quantity": 100})))
            .await;
        assert!(!out.is_error);
        assert_eq!(out.output_summary["accepted"], false);
        assert_eq!(out.output_summary["blocked"], true);
        // 闸门拦在记账之前：未调 Account、未落任何 trade。
        assert!(gw.seen_client_order_id.lock().unwrap().is_none(), "熔断中不应调 Account");
        assert!(repo.list_submitting_trades().unwrap().is_empty());
    }

    #[tokio::test]
    async fn daily_cap_only_counts_opens_and_uses_gateway_value() {
        let (records, _repo) = records_with_run();
        // 预置今日 1 笔**开仓**（[open] 标记）trade，账户 cap=1 → 第二笔开仓应被额度拒。
        let coid = RecordService::new_client_order_id();
        let t = records
            .record_trade_submitting("run1", coid.as_str(), Some(1), "seed", "[open] open_position 600519.SH")
            .unwrap();
        records.settle_trade(&t, AccountResultRef { accepted: true, ..Default::default() }).unwrap();
        // cap 来源 = 账户 gateway max_daily_new_orders()=1（不再硬编码）。
        let gw = Arc::new(MockAccount::with_cap(
            AccountResultRef { accepted: true, ..Default::default() },
            1,
        ));
        let cb = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let h = OperateAccountHandler::new(gw.clone(), quiet_quotes(), records, test_lock(), "run1", Some(1), gate(cb, true, true, false));
        let out = h
            .invoke(op_inv(json!({"action": "open_position", "tsCode": "000001.SZ", "quantity": 100})))
            .await;
        assert_eq!(out.output_summary["accepted"], false);
        assert_eq!(out.output_summary["blocked"], true);
        assert!(gw.seen_client_order_id.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn daily_cap_does_not_block_close_or_scale_when_opens_exhausted() {
        let (records, _repo) = records_with_run();
        // 当日已有 1 笔开仓，cap=1（额度耗尽）；但平仓/调仓不计、应放行（spec §6 可平/调仓）。
        let coid = RecordService::new_client_order_id();
        let t = records
            .record_trade_submitting("run1", coid.as_str(), Some(1), "seed", "[open] open_position 600519.SH")
            .unwrap();
        records.settle_trade(&t, AccountResultRef { accepted: true, ..Default::default() }).unwrap();
        let gw = Arc::new(MockAccount::with_cap(
            AccountResultRef { accepted: true, order_id: Some("ord_close".into()), ..Default::default() },
            1,
        ));
        let cb = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let h = OperateAccountHandler::new(gw.clone(), quiet_quotes(), records, test_lock(), "run1", Some(1), gate(cb, true, true, false));
        // 平仓动作：额度耗尽也应放行（不拦非开仓动作）。
        let out = h
            .invoke(op_inv(json!({"action": "close_position", "positionId": "pos1", "quantity": 100})))
            .await;
        assert_eq!(out.output_summary["accepted"], true);
        assert!(gw.seen_client_order_id.lock().unwrap().is_some(), "平仓不受额度限制，应调 Account");
    }

    #[tokio::test]
    async fn chasing_guard_downgrades_news_open_on_high_runup() {
        let (records, repo) = records_with_run();
        let gw = Arc::new(MockAccount::new(AccountResultRef { accepted: true, ..Default::default() }));
        // 当日涨幅 8% > 5% 阈值 → 开仓应被追高保护拦截。
        let quotes: Arc<dyn QuotesGateway> =
            Arc::new(MockQuotes { change_percent: 8.0, price: 10.0, limit_up: 11.0 });
        let cb = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let h = OperateAccountHandler::new(
            gw.clone(),
            quotes,
            records, test_lock(),
            "run1",
            Some(1),
            gate(cb, true, false, true), // enforce_chasing=true（news mode）
        );
        let out = h
            .invoke(op_inv(json!({"action": "open_position", "tsCode": "600519.SH", "quantity": 100})))
            .await;
        assert_eq!(out.output_summary["accepted"], false);
        assert_eq!(out.output_summary["blocked"], true);
        assert!(gw.seen_client_order_id.lock().unwrap().is_none(), "追高拦截不应调 Account");
        assert!(repo.list_submitting_trades().unwrap().is_empty());
    }

    #[tokio::test]
    async fn chasing_guard_allows_calm_stock() {
        let (records, _repo) = records_with_run();
        let gw = Arc::new(MockAccount::new(AccountResultRef { accepted: true, order_id: Some("ord2".into()), ..Default::default() }));
        // 涨幅 1%，远离涨停 → 放行。
        let quotes: Arc<dyn QuotesGateway> =
            Arc::new(MockQuotes { change_percent: 1.0, price: 10.0, limit_up: 11.0 });
        let cb = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let h = OperateAccountHandler::new(gw.clone(), quotes, records, test_lock(), "run1", Some(1), gate(cb, true, false, true));
        let out = h
            .invoke(op_inv(json!({"action": "open_position", "tsCode": "600519.SH", "quantity": 100})))
            .await;
        assert_eq!(out.output_summary["accepted"], true);
        assert!(gw.seen_client_order_id.lock().unwrap().is_some(), "平静标的应放行下单");
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
        let h = OperateAccountHandler::new(gw, quiet_quotes(), records, test_lock(), "run1", Some(1), gate(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)), true, false, false));
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
