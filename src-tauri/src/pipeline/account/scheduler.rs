//! Account 后台调度 — 定时调用 `evaluate_account_triggers`。
//!
//! Spec: docs/design/account-module.md §5 调度期望
//! - 调度层应在 `market-quotes-refreshed` 后触发评估，并使用固定 cadence 兜底。
//! - 调度间隔和 batch size 以 Agent Runtime spec 为准；这里默认 60s。

use crate::pipeline::account::eval::{evaluate_account_triggers, EvalDeps, EvalInput};
use crate::pipeline::account::service::AccountService;
use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

pub const ACCOUNT_EVAL_INTERVAL_SECS: u64 = 60;
pub const ACCOUNT_EVAL_BATCH_SIZE: usize = 200;

pub struct AccountSchedulerHandle {
    #[allow(dead_code)]
    pub eval_task: JoinHandle<()>,
}

/// 启动后台 evaluate_account_triggers 调度。
pub fn spawn_account_eval_scheduler(
    service: Arc<AccountService>,
    interval: Duration,
    batch_size: usize,
) -> AccountSchedulerHandle {
    let task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // 略过首次立即 tick — 给 setup 完成一些时间。
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let deps = EvalDeps {
                db: service.db().clone(),
                gateway: service.gateway.clone(),
                fee_policy: service.config().fee_policy.clone(),
            };
            let svc = Arc::clone(&service);
            let now = Utc::now();
            let result = tokio::task::spawn_blocking(move || {
                evaluate_account_triggers(EvalInput {
                    deps: &deps,
                    now,
                    batch_size,
                    cursor: None,
                })
            })
            .await;
            match result {
                Ok(r) => {
                    if !r.triggers.is_empty() {
                        for t in &r.triggers {
                            svc.emit_triggered(t.clone());
                        }
                    }
                    if !r.account_event_ids.is_empty() {
                        svc.emit_updated(
                            crate::pipeline::account::service::AccountUpdatedPayloadInner {
                                account_event_ids: r.account_event_ids.clone(),
                                affected_order_ids: vec![],
                                affected_position_ids: vec![],
                                affected_ts_codes: vec![],
                                affected_watchlist_ts_codes: vec![],
                                trigger_ids: r
                                    .triggers
                                    .iter()
                                    .map(|t| t.trigger_id.clone())
                                    .collect(),
                                snapshot_captured_at: now,
                            },
                        );
                    }
                    if !r.warnings.is_empty() {
                        tracing::debug!(target: "account.eval", warnings = ?r.warnings, "evaluate returned warnings");
                    }
                }
                Err(e) => {
                    tracing::error!(target: "account.eval", error = %e, "eval task panicked");
                }
            }
        }
    });
    AccountSchedulerHandle { eval_task: task }
}
