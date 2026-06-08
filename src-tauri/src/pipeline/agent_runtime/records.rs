//! 决策链记账 —— emit `AnalysisResult` / 记 `AgentTrade`（submitting→settled）/ orderId→runId 索引。
//!
//! Spec: docs/design/agent-runtime-module.md §3 `AnalysisResult` / `AgentTrade`
//!
//! - news mode 形成判断必 emit AnalysisResult（含 no_action）。
//! - 每次 operate_account 先落 `submitting`（崩溃锚点），拿到结果转 `settled`；
//!   `accepted` 且有 `order_id` 时写 `orderId→runId` 反查索引（account_trigger 归因）。

use std::sync::{Arc, RwLock};

use chrono::Utc;
use uuid::Uuid;

use crate::domain::agent::runtime::{
    AccountResultRef, AgentTrade, AgentTradeStatus, AnalysisResult, AnalysisResultKind,
    ReviewSuggestion,
};
use crate::domain::shared::{TradeDate, TsCode};
use crate::infrastructure::agent::runtime_repo::AgentRuntimeRepo;

use super::runs::{RuntimeEvent, RuntimeEventSink};

pub struct RecordService {
    repo: Arc<AgentRuntimeRepo>,
    event_sink: RwLock<Option<RuntimeEventSink>>,
}

impl RecordService {
    pub fn new(repo: Arc<AgentRuntimeRepo>) -> Self {
        Self {
            repo,
            event_sink: RwLock::new(None),
        }
    }

    pub fn set_event_sink(&self, sink: RuntimeEventSink) {
        if let Ok(mut g) = self.event_sink.write() {
            *g = Some(sink);
        }
    }

    fn emit(&self, ev: RuntimeEvent) {
        if let Ok(g) = self.event_sink.read() {
            if let Some(sink) = g.as_ref() {
                sink(ev);
            }
        }
    }

    /// 自 `since` 起的 AgentTrades（防自我打架 L3 注入用，spec §6）。
    pub fn list_trades_since(
        &self,
        since: chrono::DateTime<Utc>,
    ) -> rusqlite::Result<Vec<AgentTrade>> {
        self.repo.list_trades_since(since)
    }

    /// 本 run 已记的 AgentTrades（record_analysis 关联 tradeIds 用，spec §3 产生机制）。
    pub fn list_trades_by_run(&self, run_id: &str) -> rusqlite::Result<Vec<AgentTrade>> {
        self.repo.list_trades_by_run(run_id)
    }

    /// news mode 分析产出 —— 落库 + emit `agent-analysis-result`。
    pub fn record_analysis_result(
        &self,
        run_id: &str,
        kind: AnalysisResultKind,
        summary: impl Into<String>,
        related_codes: Vec<TsCode>,
        trade_ids: Vec<String>,
    ) -> rusqlite::Result<AnalysisResult> {
        let result = AnalysisResult {
            result_id: format!("ar_{}", Uuid::new_v4()),
            run_id: run_id.to_string(),
            kind,
            summary: summary.into(),
            related_codes,
            trade_ids,
            created_at: Utc::now(),
        };
        self.repo.insert_analysis_result(&result)?;
        self.emit(RuntimeEvent::AnalysisResultEmitted {
            result_id: result.result_id.clone(),
            run_id: result.run_id.clone(),
            kind: result.kind,
        });
        Ok(result)
    }

    /// 落一条复盘策略建议（review run `record_review_suggestion` 工具，spec §3 ④）。
    pub fn record_review_suggestion(
        &self,
        review_run_id: &str,
        trade_date: TradeDate,
        text: impl Into<String>,
    ) -> rusqlite::Result<ReviewSuggestion> {
        let s = ReviewSuggestion {
            suggestion_id: format!("rs_{}", Uuid::new_v4()),
            review_run_id: review_run_id.to_string(),
            trade_date,
            text: text.into(),
            created_at: Utc::now(),
        };
        self.repo.insert_review_suggestion(&s)?;
        Ok(s)
    }

    /// 读某交易日的复盘建议（下次 review follow-up 对账用，spec §3 ④）。
    pub fn list_review_suggestions_by_date(
        &self,
        trade_date: &TradeDate,
    ) -> rusqlite::Result<Vec<ReviewSuggestion>> {
        self.repo.list_review_suggestions_by_date(trade_date)
    }

    /// 调 Account 前落 `submitting`（崩溃锚点）。`client_order_id` 由 Runtime 单点生成。
    pub fn record_trade_submitting(
        &self,
        run_id: &str,
        client_order_id: impl Into<String>,
        strategy_version: Option<u32>,
        reason: impl Into<String>,
        account_input_summary: impl Into<String>,
    ) -> rusqlite::Result<AgentTrade> {
        let now = Utc::now();
        let trade = AgentTrade {
            trade_id: format!("td_{}", Uuid::new_v4()),
            run_id: run_id.to_string(),
            client_order_id: client_order_id.into(),
            strategy_version,
            reason: reason.into(),
            account_input_summary: account_input_summary.into(),
            status: AgentTradeStatus::Submitting,
            account_result_ref: None,
            created_at: now,
            updated_at: now,
        };
        self.repo.insert_trade_submitting(&trade)?;
        Ok(trade)
    }

    /// 生成一个 client_order_id（Runtime 单点生成，传给 Account 去重 + 恢复对账）。
    pub fn new_client_order_id() -> String {
        format!("co_{}", Uuid::new_v4())
    }

    /// 拿到 Account 结果后转 `settled`；`accepted` 且有 `order_id` → 写 orderId→runId 索引。
    pub fn settle_trade(
        &self,
        trade: &AgentTrade,
        result: AccountResultRef,
    ) -> rusqlite::Result<()> {
        let now = Utc::now();
        self.repo.settle_trade(&trade.trade_id, &result, now)?;
        if result.accepted {
            if let Some(order_id) = result.order_id.as_deref() {
                self.repo.upsert_order_run_index(
                    order_id,
                    &trade.run_id,
                    &trade.trade_id,
                    &trade.client_order_id,
                    now,
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::channel::WireFormat;
    use crate::domain::agent::runtime::{AgentRun, AgentRunStatus, AgentRunTrigger};
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::db::{run_migrations, AppDb};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn setup() -> (RecordService, Arc<AgentRuntimeRepo>) {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        let repo = Arc::new(AgentRuntimeRepo::new(db));
        // 需要一个 run 作 FK 语义（虽无 FK 约束，逻辑上挂 run_id）。
        repo.insert_run(
            &AgentRun {
                run_id: "run1".into(),
                mode: crate::domain::agent::runtime::AgentRunMode::News,
                trigger: AgentRunTrigger::NewsBatch { news_ids: vec!["n1".into()] },
                parent_run_id: None,
                provider: "anthropic".into(),
                wire_format: WireFormat::Messages,
                model: "claude".into(),
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
        (RecordService::new(repo.clone()), repo)
    }

    #[test]
    fn analysis_result_emits_event() {
        let (svc, _repo) = setup();
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        svc.set_event_sink(Arc::new(move |ev| {
            if let RuntimeEvent::AnalysisResultEmitted { kind, .. } = ev {
                assert_eq!(kind, AnalysisResultKind::NoAction);
                n2.fetch_add(1, Ordering::SeqCst);
            }
        }));
        let r = svc
            .record_analysis_result(
                "run1",
                AnalysisResultKind::NoAction,
                "已 price-in，观望",
                vec![TsCode::parse("600519.SH").unwrap()],
                vec![],
            )
            .unwrap();
        assert!(r.result_id.starts_with("ar_"));
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn trade_submitting_then_settle_accepted_writes_order_index() {
        let (svc, repo) = setup();
        let trade = svc
            .record_trade_submitting("run1", "co1", Some(1), "开仓", "buy 600519 100")
            .unwrap();
        assert_eq!(repo.list_submitting_trades().unwrap().len(), 1);

        svc.settle_trade(
            &trade,
            AccountResultRef {
                accepted: true,
                order_id: Some("ord1".into()),
                fill_ids: vec!["f1".into()],
                position_id: Some("pos1".into()),
                account_event_ids: vec!["e1".into()],
                rejection_event_id: None,
                reason: None,
                message: None,
            },
        )
        .unwrap();
        assert_eq!(repo.list_submitting_trades().unwrap().len(), 0);
        // accepted + order_id → 索引可反查到 run/trade。
        assert_eq!(
            repo.find_run_by_order_id("ord1").unwrap(),
            Some(("run1".into(), trade.trade_id.clone()))
        );
    }

    #[test]
    fn settle_rejected_no_order_index() {
        let (svc, repo) = setup();
        let trade = svc
            .record_trade_submitting("run1", "co2", Some(1), "开仓", "buy 600519 100")
            .unwrap();
        svc.settle_trade(
            &trade,
            AccountResultRef {
                accepted: false,
                order_id: None,
                fill_ids: vec![],
                position_id: None,
                account_event_ids: vec![],
                rejection_event_id: Some("rej1".into()),
                reason: Some(crate::domain::shared::ErrorCode::QuoteStale),
                message: Some("行情过期".into()),
            },
        )
        .unwrap();
        // 被拒不写 order 索引。
        assert!(repo.find_run_by_order_id("ord1").unwrap().is_none());
        // 但 trade 已 settled（记录被拒事实）。
        assert_eq!(repo.list_submitting_trades().unwrap().len(), 0);
    }

    #[test]
    fn client_order_id_unique_prefix() {
        let a = RecordService::new_client_order_id();
        let b = RecordService::new_client_order_id();
        assert!(a.starts_with("co_") && b.starts_with("co_"));
        assert_ne!(a, b);
    }
}
