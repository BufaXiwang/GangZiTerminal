//! Run cancellation.
//!
//! Spec: docs/design/agent-runtime-module.md §9 cancel_agent_run

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
}
