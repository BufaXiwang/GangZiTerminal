//! Risk monitoring + circuit breaker + run cancellation。
//!
//! Spec: docs/design/agent-runtime-module.md §6 熔断 / §9 cancel_agent_run

use std::sync::atomic::Ordering;

use chrono::Utc;

use crate::domain::agent::runtime::AgentRunStatus;

use super::RuntimeServices;

/// cancel_agent_run 判定（spec §9 `CancelAgentRunResponse`）。
#[derive(Debug, Clone, Copy)]
pub struct CancelRunOutcome {
    pub accepted: bool,
    pub status: CancelRunStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelRunStatus {
    Cancelled,
    Completed,
    Failed,
    NotFound,
}

impl RuntimeServices {
    /// 取消一个在跑的 run（cancel_agent_run）。返回是否找到并取消。
    pub fn cancel_run(&self, run_id: &str) -> bool {
        if let Ok(g) = self.cancel_registry.lock() {
            if let Some(tok) = g.get(run_id) {
                tok.cancel();
                return true;
            }
        }
        false
    }

    /// 取消一个 run，返回 spec §9 `CancelAgentRunResponse` 的判定。
    pub fn cancel_run_detailed(&self, run_id: &str) -> CancelRunOutcome {
        if self.cancel_run(run_id) {
            return CancelRunOutcome { accepted: true, status: CancelRunStatus::Cancelled };
        }
        match self.runtime_repo.get_run(run_id) {
            Ok(Some(run)) => {
                let status = match run.status {
                    AgentRunStatus::Cancelled => CancelRunStatus::Cancelled,
                    AgentRunStatus::Completed => CancelRunStatus::Completed,
                    AgentRunStatus::Failed => CancelRunStatus::Failed,
                    AgentRunStatus::Queued | AgentRunStatus::Running => CancelRunStatus::Failed,
                };
                let accepted = matches!(status, CancelRunStatus::Cancelled);
                CancelRunOutcome { accepted, status }
            }
            _ => CancelRunOutcome { accepted: false, status: CancelRunStatus::NotFound },
        }
    }

    /// 解除/恢复熔断（spec §9 set_circuit_breaker）。
    pub fn set_circuit_breaker(&self, resume: bool) -> bool {
        if resume {
            self.flip_circuit_breaker(false);
        }
        self.circuit_breaker_active()
    }

    /// 内部翻转熔断状态并 emit 可观测。
    pub(super) fn flip_circuit_breaker(&self, active: bool) {
        let prev = self.circuit_breaker.swap(active, Ordering::Relaxed);
        if prev != active {
            if let Err(e) = self.settings.set_circuit_breaker_active(active) {
                tracing::warn!(
                    target: "runtime.circuit_breaker",
                    error = %e,
                    active,
                    "persist circuit_breaker_active failed"
                );
            }
            if let Some(sink) = self.circuit_breaker_sink.as_ref() {
                sink(
                    active,
                    if active {
                        "熔断激活".to_string()
                    } else {
                        "熔断解除".to_string()
                    },
                );
            }
        }
    }

    /// 当前熔断是否激活。
    pub fn circuit_breaker_active(&self) -> bool {
        self.circuit_breaker.load(Ordering::Relaxed)
    }

    /// 自动熔断监控（spec §6）。
    pub async fn monitor_risk(&self) -> Option<String> {
        if self.circuit_breaker_active() {
            return None;
        }
        let now = Utc::now();
        let losses = self.deps.account.consecutive_losses(now);
        let drawdown = self.deps.account.daily_drawdown(now);

        let reason = super::super::risk::circuit_breaker_tripped(losses, drawdown, &self.risk)?;
        self.flip_circuit_breaker(true);
        tracing::warn!(
            target: "runtime.circuit_breaker",
            losses,
            drawdown,
            reason = %reason,
            "自动熔断激活（需对话确认解除）"
        );
        Some(reason)
    }
}
