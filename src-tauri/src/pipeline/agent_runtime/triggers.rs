//! Trigger 路由 —— 跨 BC 事件消费幂等 + account-triggered 的 orderId→runId 归因。
//!
//! Spec: docs/design/agent-runtime-module.md §6 编排流 / §8 幂等与可靠性
//!
//! - 事件消费幂等：`(event_type, event_key, consumer="runtime")` 键；已 consumed/processing → 不重触发。
//! - account-triggered：去重 `trigger_id` → 起 account_trigger run → 经 `orderId→runId` 归因到原始
//!   建仓 run（写 `causation_run_id`）→ run 终态后 `mark_consumed`（调用方随后 Account.mark_trigger_handled）。
//! - `mark_trigger_handled` 只在 consumption 进终态后调用（仅启动 run 不得标 handled）。

use std::sync::Arc;

use chrono::Utc;

use crate::infrastructure::agent::runtime_repo::AgentRuntimeRepo;

pub const CONSUMER: &str = "runtime";
pub const EV_ACCOUNT_TRIGGERED: &str = "account-triggered";
pub const EV_NEWS_REFRESHED: &str = "news-refreshed";
// 注：`market-quotes-refreshed` 不再被 Runtime 当作 durable consumption 事件消费——
// 账户走自有 focused refresh quote tick 自驱（spec agent-runtime §6/§8），universe refreshed 退化为纯 UI 事件。
/// 收盘复盘「一日一次」durable 幂等锁（spec §6/§8：key=tradeDate，重启不重复跑、也不漏）。
pub const EV_EOD_REVIEW: &str = "eod_review";

/// 归因结果：account_trigger run 关联到的原始建仓 run（找不到映射时 None + mapping_missing 语义）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attribution {
    /// orderId → 原始建仓 run。
    Origin { run_id: String, trade_id: String },
    /// trigger 无 orderId（市价即时成交/保护调整等无订单终态），不归因。
    NoOrder,
    /// 有 orderId 但反查不到（mapping_missing：仍处理，但归因缺失）。
    MappingMissing,
}

pub struct TriggerRouter {
    repo: Arc<AgentRuntimeRepo>,
}

impl TriggerRouter {
    pub fn new(repo: Arc<AgentRuntimeRepo>) -> Self {
        Self { repo }
    }

    /// 尝试开始消费某事件。返回 false = 已 consumed/ignored/processing（不重复触发）。
    pub fn begin(&self, event_type: &str, event_key: &str) -> rusqlite::Result<bool> {
        self.repo.begin_consumption(event_type, event_key, CONSUMER, Utc::now())
    }

    pub fn mark_consumed(
        &self,
        event_type: &str,
        event_key: &str,
        run_id: Option<&str>,
    ) -> rusqlite::Result<()> {
        self.repo
            .mark_consumption(event_type, event_key, CONSUMER, "consumed", run_id, None, Utc::now())
    }

    pub fn mark_ignored(&self, event_type: &str, event_key: &str) -> rusqlite::Result<()> {
        self.repo
            .mark_consumption(event_type, event_key, CONSUMER, "ignored", None, None, Utc::now())
    }

    pub fn mark_failed(
        &self,
        event_type: &str,
        event_key: &str,
        error: &str,
    ) -> rusqlite::Result<()> {
        self.repo.mark_consumption(
            event_type,
            event_key,
            CONSUMER,
            "failed",
            None,
            Some(error),
            Utc::now(),
        )
    }

    /// account-triggered 去重（按 trigger_id）。
    pub fn begin_account_trigger(&self, trigger_id: &str) -> rusqlite::Result<bool> {
        self.begin(EV_ACCOUNT_TRIGGERED, trigger_id)
    }

    /// account-triggered 消费成功终态（run 跑完后调；记关联 run_id）。
    pub fn mark_account_trigger_consumed(
        &self,
        trigger_id: &str,
        run_id: Option<&str>,
    ) -> rusqlite::Result<()> {
        self.mark_consumed(EV_ACCOUNT_TRIGGERED, trigger_id, run_id)
    }

    /// account-triggered 消费失败终态（run 出错后调）。
    pub fn mark_account_trigger_failed(
        &self,
        trigger_id: &str,
        error: &str,
    ) -> rusqlite::Result<()> {
        self.mark_failed(EV_ACCOUNT_TRIGGERED, trigger_id, error)
    }

    /// 把新建的 account_trigger run 归因到原始建仓 run（spec §6）。
    ///
    /// `order_id=Some` 且反查命中 → 写 `causation_run_id`、返回 `Origin`；
    /// 反查不到 → `MappingMissing`（仍处理 trigger，但归因缺失）；
    /// `order_id=None`（无订单终态）→ `NoOrder`。
    pub fn attribute_account_trigger(
        &self,
        new_run_id: &str,
        order_id: Option<&str>,
    ) -> rusqlite::Result<Attribution> {
        let Some(order_id) = order_id else {
            return Ok(Attribution::NoOrder);
        };
        match self.repo.find_run_by_order_id(order_id)? {
            Some((origin_run_id, trade_id)) => {
                self.repo.set_causation_run(new_run_id, &origin_run_id)?;
                Ok(Attribution::Origin {
                    run_id: origin_run_id,
                    trade_id,
                })
            }
            None => Ok(Attribution::MappingMissing),
        }
    }

    /// 在**起 run 之前**先按 `order_id` 反查原始建仓 run（spec §3 mode 表 / §6：account_trigger 的
    /// L3 需注入「原始建仓 run 摘要」，故反查须前置于 `execute_run`，让模型看得到原单上下文）。
    ///
    /// 与 `attribute_account_trigger` 区别：本方法**不写** causation（此刻新 run 尚未创建）——只解析归因。
    /// `MappingMissing`（有 orderId 但反查不到）经 `tracing::warn!` + heartbeat 记可观测（spec §3），不静默。
    pub fn resolve_attribution(&self, order_id: Option<&str>) -> rusqlite::Result<Attribution> {
        let Some(order_id) = order_id else {
            return Ok(Attribution::NoOrder);
        };
        match self.repo.find_run_by_order_id(order_id)? {
            Some((origin_run_id, trade_id)) => Ok(Attribution::Origin {
                run_id: origin_run_id,
                trade_id,
            }),
            None => {
                // 记可观测：warn + heartbeat（spec §3，不静默丢弃）。
                tracing::warn!(
                    target: "runtime.account_trigger",
                    order_id = %order_id,
                    "mapping_missing：account_trigger 的 orderId 反查不到原始建仓 run，归因缺失"
                );
                let _ = self.repo.record_heartbeat_error(
                    "account_trigger_mapping",
                    &format!("mapping_missing order_id={order_id}"),
                    Utc::now(),
                );
                Ok(Attribution::MappingMissing)
            }
        }
    }

    /// 写 causation（run 创建后调；resolve_attribution 已确认 origin）。
    pub fn set_causation(&self, new_run_id: &str, origin_run_id: &str) -> rusqlite::Result<()> {
        self.repo.set_causation_run(new_run_id, origin_run_id)
    }

    /// 拉取「原始建仓 run 摘要」文本（spec §3/§6）：原 run 的 mode/trigger/起止 + 其 AnalysisResults +
    /// AgentTrades 摘要。供 account_trigger run 的 L3 注入，让模型看到原始建仓判断链。
    pub fn origin_run_summary(&self, origin_run_id: &str) -> rusqlite::Result<String> {
        let mut out = String::new();
        match self.repo.get_run(origin_run_id)? {
            Some(run) => {
                out.push_str(&format!(
                    "原始建仓 run：run_id={} mode={:?} trigger={:?}",
                    run.run_id, run.mode, run.trigger
                ));
                if let Some(s) = run.started_at {
                    out.push_str(&format!(" 起={}", s.to_rfc3339()));
                }
                if let Some(e) = run.ended_at {
                    out.push_str(&format!(" 止={}", e.to_rfc3339()));
                }
                out.push('\n');
            }
            None => {
                out.push_str(&format!("原始建仓 run {origin_run_id}（记录缺失）\n"));
            }
        }
        let results = self.repo.list_analysis_results_by_run(origin_run_id)?;
        if !results.is_empty() {
            out.push_str("分析结论：\n");
            for r in &results {
                out.push_str(&format!(
                    "· [{}] {}\n",
                    if matches!(
                        r.kind,
                        crate::domain::agent::runtime::AnalysisResultKind::Action
                    ) {
                        "action"
                    } else {
                        "no_action"
                    },
                    r.summary,
                ));
            }
        }
        let trades = self.repo.list_trades_by_run(origin_run_id)?;
        if !trades.is_empty() {
            out.push_str("下单记录：\n");
            for t in &trades {
                out.push_str(&format!(
                    "· {} —— {}（status={:?}）\n",
                    t.account_input_summary, t.reason, t.status
                ));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::channel::WireFormat;
    use crate::domain::agent::runtime::{AgentRun, AgentRunMode, AgentRunStatus, AgentRunTrigger};
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::db::{run_migrations, AppDb};

    fn router() -> (TriggerRouter, Arc<AgentRuntimeRepo>) {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        let repo = Arc::new(AgentRuntimeRepo::new(db));
        (TriggerRouter::new(repo.clone()), repo)
    }

    fn run(repo: &AgentRuntimeRepo, id: &str) {
        repo.insert_run(
            &AgentRun {
                run_id: id.into(),
                mode: AgentRunMode::News,
                trigger: AgentRunTrigger::NewsBatch { news_ids: vec![] },
                parent_run_id: None,
                provider: "p".into(),
                wire_format: WireFormat::Messages,
                model: "m".into(),
                strategy_version: None,
                causation_run_id: None,
                status: AgentRunStatus::Running,
                started_at: None,
                ended_at: None,
                error: None,
            },
            Utc::now(),
        )
        .unwrap();
    }

    #[test]
    fn account_trigger_dedup() {
        let (r, _repo) = router();
        assert!(r.begin_account_trigger("tg1").unwrap()); // 首次
        assert!(!r.begin_account_trigger("tg1").unwrap()); // processing 中，不重触发
        r.mark_consumed(EV_ACCOUNT_TRIGGERED, "tg1", Some("run1")).unwrap();
        assert!(!r.begin_account_trigger("tg1").unwrap()); // consumed 后仍不触发
    }

    #[test]
    fn attribution_origin_sets_causation() {
        let (r, repo) = router();
        run(&repo, "origin_run"); // 建仓 run
        run(&repo, "trigger_run"); // 新 account_trigger run
        repo.upsert_order_run_index("ord1", "origin_run", "td1", "co1", Utc::now())
            .unwrap();

        let attr = r.attribute_account_trigger("trigger_run", Some("ord1")).unwrap();
        assert_eq!(
            attr,
            Attribution::Origin { run_id: "origin_run".into(), trade_id: "td1".into() }
        );
        // 新 run 的 causation 已写。
        assert_eq!(
            repo.get_run("trigger_run").unwrap().unwrap().causation_run_id,
            Some("origin_run".into())
        );
    }

    #[test]
    fn attribution_no_order_and_mapping_missing() {
        let (r, repo) = router();
        run(&repo, "trigger_run");
        // 无 orderId（保护调整/市价即时成交）→ NoOrder。
        assert_eq!(
            r.attribute_account_trigger("trigger_run", None).unwrap(),
            Attribution::NoOrder
        );
        // 有 orderId 但反查不到 → MappingMissing。
        assert_eq!(
            r.attribute_account_trigger("trigger_run", Some("unknown")).unwrap(),
            Attribution::MappingMissing
        );
    }
}
