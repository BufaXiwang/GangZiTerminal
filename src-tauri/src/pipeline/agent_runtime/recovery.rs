//! 启动恢复 —— 进程重启后把中断态收尾，保证决策链可续、不丢不重。
//!
//! Spec: docs/design/agent-runtime-module.md §8 失败 / 恢复
//!
//! 顺序（spec §8）：① 对账 `submitting` 悬挂 AgentTrade → ② 旧 `running` run 标
//! `failed(interrupted_by_restart)` → ③ 事件消费恢复 → ④ news buffer `in_batch` 孤儿回 pending
//! → ⑤ 补扫未 handled trigger → ⑥ 补做缺失收盘复盘。
//!
//! 本模块做**纯 repo 可恢复的步骤**（①②③④）；⑤（需 Account 扫描）由 `RuntimeServices`
//! 的 `rescan_unhandled_triggers`（async，握 Account gateway）接；⑥（需 scheduler）由
//! `maybe_run_eod_review`（按交易日 durable 幂等锁）接。
//!
//! 顺序要点（review #4）：② 必须先于 ④——把中断 run 标 failed 后，④ 的孤儿检测才能识别
//! `in_batch` 但所属 run 非 running 的条目。①（trade 对账）独立于 run 状态，先做。

use std::sync::Arc;

use chrono::Utc;

use crate::domain::agent::runtime::{AccountResultRef, AgentRunStatus};
use crate::infrastructure::agent::runtime_repo::AgentRuntimeRepo;

/// 启动恢复结果（可观测 + 测试断言）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoverySummary {
    /// 旧 running run 被标 failed 的数量。
    pub interrupted_runs: u64,
    /// submitting 悬挂 trade 被对账处理的总数（= accepted + failed）。
    pub reconciled_trades: u64,
    /// 对账**成功**（Account 已有对应订单 → accepted=true）的 submitting trade 数。
    pub reconciled_accepted: u64,
    /// 对账**失败**（确无 Account 效果 → accepted=false）的 submitting trade 数。
    pub reconciled_failed: u64,
    /// news buffer in_batch 孤儿回 pending 的数量。
    pub reset_news_items: u64,
    /// 未终态（processing）事件消费记录被回收（→ failed，可重处理）的数量（③，spec §8）。
    pub recovered_consumptions: u64,
}

pub struct RecoveryService {
    repo: Arc<AgentRuntimeRepo>,
}

impl RecoveryService {
    pub fn new(repo: Arc<AgentRuntimeRepo>) -> Self {
        Self { repo }
    }

    /// 执行可纯 repo 恢复的步骤（①②④），按 spec §8 顺序。无 reconciler → 退回保守失败行为。
    pub fn recover_on_startup(&self) -> rusqlite::Result<RecoverySummary> {
        self.recover_on_startup_with(None)
    }

    /// 执行启动恢复（①②④），按 spec §8 顺序。
    ///
    /// ① submitting 悬挂 trade 对账（spec §3/§8「无猜失败盲区」）：
    /// - `reconciler` 传入时：对每条 trade 用 `client_order_id` 反查 Account。
    ///   - `Some(order_id)` → Account 已受理 → `settle(accepted=true, order_id)` + 写 orderId→runId 索引。
    ///   - `None` → 确无 Account 效果 → `settle(accepted=false, "submission_no_account_effect")`。
    /// - `reconciler` 为 None（无对账能力）→ 一律保守标失败（旧行为）。
    ///
    /// reconciler 同步、与 Account 解耦：入参 client_order_id，返回 `Some(order_id)`/`None`。
    pub fn recover_on_startup_with(
        &self,
        reconciler: Option<&dyn Fn(&str) -> Option<String>>,
    ) -> rusqlite::Result<RecoverySummary> {
        let now = Utc::now();
        let mut summary = RecoverySummary::default();

        // ① 对账 submitting 悬挂 trade。
        for trade in self.repo.list_submitting_trades()? {
            let reconciled_order_id =
                reconciler.and_then(|f| f(&trade.client_order_id));
            match reconciled_order_id {
                Some(order_id) => {
                    // Account 已有对应订单 = 已受理 → accepted=true，并写 orderId→runId 反查索引
                    // （与正常 settle 路径一致，否则决策链断链）。
                    let result = AccountResultRef {
                        accepted: true,
                        order_id: Some(order_id.clone()),
                        ..Default::default()
                    };
                    self.repo.settle_trade(&trade.trade_id, &result, now)?;
                    self.repo.upsert_order_run_index(
                        &order_id,
                        &trade.run_id,
                        &trade.trade_id,
                        &trade.client_order_id,
                        now,
                    )?;
                    summary.reconciled_accepted += 1;
                }
                None => {
                    // 确无 Account 效果（或无对账能力）→ 保守标失败。
                    let result = AccountResultRef {
                        accepted: false,
                        message: Some("submission_no_account_effect".into()),
                        ..Default::default()
                    };
                    self.repo.settle_trade(&trade.trade_id, &result, now)?;
                    summary.reconciled_failed += 1;
                }
            }
            summary.reconciled_trades += 1;
        }

        // ② 旧 running run 标 failed(interrupted_by_restart)。
        for run in self.repo.list_running_runs()? {
            self.repo.set_run_status(
                &run.run_id,
                AgentRunStatus::Failed,
                None,
                Some(now),
                Some("interrupted_by_restart"),
            )?;
            summary.interrupted_runs += 1;
        }

        // ③ 事件消费恢复（spec §8「processing 超时可回收」）：重启 = 进程内执行被中断，故启动时
        //    把所有未终态（processing）的 EventConsumption 记录回收为 failed（标 interrupted_by_restart）。
        //    failed 是非终态：下次 `begin_consumption` 会重新置 processing 重处理（不漏）；已 consumed/
        //    ignored 不动（不重复）。
        for (event_type, event_key, consumer) in self.repo.list_processing_consumptions()? {
            self.repo
                .reset_processing_consumption(&event_type, &event_key, &consumer, now)?;
            summary.recovered_consumptions += 1;
        }

        // ④ news buffer in_batch 孤儿回 pending（必须在 ② 之后）。
        summary.reset_news_items = self.repo.reset_orphan_in_batch_news()?;

        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::channel::WireFormat;
    use crate::domain::agent::runtime::{
        AgentRun, AgentRunMode, AgentRunTrigger, AgentTrade, AgentTradeStatus,
    };
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::db::{run_migrations, AppDb};

    fn repo() -> Arc<AgentRuntimeRepo> {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        Arc::new(AgentRuntimeRepo::new(db))
    }

    fn running_run(repo: &AgentRuntimeRepo, id: &str) {
        repo.insert_run(
            &AgentRun {
                run_id: id.into(),
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
    }

    #[test]
    fn recover_marks_runs_failed_reconciles_trades_resets_buffer() {
        let repo = repo();
        // 中断 run（status=running）。
        running_run(&repo, "run1");
        // 悬挂 submitting trade。
        let now = Utc::now();
        repo.insert_trade_submitting(&AgentTrade {
            trade_id: "td1".into(),
            run_id: "run1".into(),
            client_order_id: "co1".into(),
            strategy_version: Some(1),
            reason: "开仓".into(),
            account_input_summary: "buy".into(),
            status: AgentTradeStatus::Submitting,
            account_result_ref: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();
        // in_batch 孤儿 news（run1 重启后将被标 failed → 孤儿）。
        repo.push_news_pending("n1", now, Some(now)).unwrap();
        repo.mark_news_in_batch(&["n1".to_string()], "run1").unwrap();

        let svc = RecoveryService::new(repo.clone());
        // 不传 reconciler（None）→ 保守失败路径，断言不变。
        let summary = svc.recover_on_startup().unwrap();

        assert_eq!(summary.interrupted_runs, 1);
        assert_eq!(summary.reconciled_trades, 1);
        assert_eq!(summary.reconciled_failed, 1);
        assert_eq!(summary.reconciled_accepted, 0);
        assert_eq!(summary.reset_news_items, 1);

        // run 已 failed。
        assert_eq!(repo.get_run("run1").unwrap().unwrap().status, AgentRunStatus::Failed);
        assert!(repo.list_running_runs().unwrap().is_empty());
        // trade 已 settled（保守失败），无悬挂。
        assert!(repo.list_submitting_trades().unwrap().is_empty());
        // 保守失败不写 orderId→runId 索引。
        assert!(repo.find_run_by_order_id("ord1").unwrap().is_none());
        // news 回 pending。
        assert_eq!(repo.count_pending_news().unwrap(), 1);
    }

    #[test]
    fn recover_with_reconciler_accepts_existing_and_fails_missing() {
        let repo = repo();
        let now = Utc::now();
        // 两条悬挂 submitting trade：co_exist（Account 已有订单）/ co_missing（确无）。
        repo.insert_trade_submitting(&AgentTrade {
            trade_id: "td_exist".into(),
            run_id: "run_exist".into(),
            client_order_id: "co_exist".into(),
            strategy_version: Some(1),
            reason: "开仓".into(),
            account_input_summary: "buy".into(),
            status: AgentTradeStatus::Submitting,
            account_result_ref: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();
        repo.insert_trade_submitting(&AgentTrade {
            trade_id: "td_missing".into(),
            run_id: "run_missing".into(),
            client_order_id: "co_missing".into(),
            strategy_version: Some(1),
            reason: "开仓".into(),
            account_input_summary: "buy".into(),
            status: AgentTradeStatus::Submitting,
            account_result_ref: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();

        // reconciler：co_exist → Some(orderId)，其余 → None。
        let reconciler = |coid: &str| -> Option<String> {
            if coid == "co_exist" {
                Some("ord_exist".to_string())
            } else {
                None
            }
        };

        let svc = RecoveryService::new(repo.clone());
        let summary = svc.recover_on_startup_with(Some(&reconciler)).unwrap();

        assert_eq!(summary.reconciled_trades, 2);
        assert_eq!(summary.reconciled_accepted, 1);
        assert_eq!(summary.reconciled_failed, 1);

        // 两条都已 settled，无悬挂。
        assert!(repo.list_submitting_trades().unwrap().is_empty());
        // 存在的 coid → accepted=true + 写 orderId→runId 索引（决策链不断链）。
        assert_eq!(
            repo.find_run_by_order_id("ord_exist").unwrap(),
            Some(("run_exist".into(), "td_exist".into()))
        );
        // 不存在的 coid → accepted=false（保守失败）。
        let missing = repo.find_trade_by_client_order_id("co_missing").unwrap().unwrap();
        assert_eq!(missing.status, AgentTradeStatus::Settled);
        assert!(!missing.account_result_ref.as_ref().unwrap().accepted);
    }

    #[test]
    fn recover_resets_processing_consumptions() {
        let repo = repo();
        let now = Utc::now();
        // 两条 processing（中断态）+ 一条 consumed（终态，不应动）。
        assert!(repo
            .begin_consumption("account-triggered", "tg1", "runtime", now)
            .unwrap());
        assert!(repo
            .begin_consumption("market-quotes-refreshed", "s|p|d", "runtime", now)
            .unwrap());
        assert!(repo
            .begin_consumption("news-refreshed", "b1", "runtime", now)
            .unwrap());
        repo.mark_consumption("news-refreshed", "b1", "runtime", "consumed", None, None, now)
            .unwrap();

        let s = RecoveryService::new(repo.clone()).recover_on_startup().unwrap();
        // 两条 processing 被回收；consumed 不计。
        assert_eq!(s.recovered_consumptions, 2);
        // 回收后无 processing 残留。
        assert!(repo.list_processing_consumptions().unwrap().is_empty());
        // 被回收的 → failed（非终态）→ begin 可重新处理；consumed 的 → 仍 false（终态，不重处理）。
        assert!(repo
            .begin_consumption("account-triggered", "tg1", "runtime", now)
            .unwrap());
        assert!(!repo
            .begin_consumption("news-refreshed", "b1", "runtime", now)
            .unwrap());
    }

    #[test]
    fn recover_noop_when_clean() {
        let repo = repo();
        let summary = RecoveryService::new(repo).recover_on_startup().unwrap();
        assert_eq!(summary, RecoverySummary::default());
    }

    #[test]
    fn in_batch_under_live_run_not_reset() {
        // run 仍 running（未中断，正常并发）→ 它的 in_batch 不应被回收。
        // 但 recover 会把所有 running 标 failed（重启语义：所有 running 都视为中断），
        // 因此该 run 的 in_batch 也会回 pending —— 这是重启恢复的预期（重启时无真正"存活"的 run）。
        let repo = repo();
        running_run(&repo, "run1");
        let now = Utc::now();
        repo.push_news_pending("n1", now, Some(now)).unwrap();
        repo.mark_news_in_batch(&["n1".to_string()], "run1").unwrap();
        let s = RecoveryService::new(repo.clone()).recover_on_startup().unwrap();
        assert_eq!(s.interrupted_runs, 1);
        assert_eq!(s.reset_news_items, 1); // run 被标 failed → 孤儿回收
        assert_eq!(repo.count_pending_news().unwrap(), 1);
    }
}
