//! Agent state query — fetch_agent_state 快照。
//!
//! Spec: docs/design/agent-runtime-module.md §9 fetch_agent_state

use crate::domain::agent::runtime::{AgentRun, AnalysisResult, InvestmentStrategy};

use super::{OrchestrationError, RuntimeServices};

/// fetch_agent_state 的 include 选择器（spec §9）；默认 = 现有 4 段。
#[derive(Debug, Clone, Copy)]
pub struct StateInclude {
    pub strategy: bool,
    pub runs: bool,
    pub analysis_results: bool,
    pub trades: bool,
    pub messages: bool,
    pub tool_calls: bool,
}

impl Default for StateInclude {
    fn default() -> Self {
        Self {
            strategy: true,
            runs: true,
            analysis_results: true,
            trades: false,
            messages: false,
            tool_calls: false,
        }
    }
}

/// Agent 总览快照（spec §9 fetch_agent_state）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct AgentStateSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_strategy: Option<InvestmentStrategy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_runs: Vec<AgentRun>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_results: Vec<AnalysisResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_trades: Vec<crate::domain::agent::runtime::AgentTrade>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_messages: Vec<crate::domain::agent::AgentMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_tool_calls: Vec<crate::domain::agent::ToolCall>,
}

impl RuntimeServices {
    /// 前端总览快照（向后兼容入口）。
    pub fn fetch_state(&self, limit: u32) -> Result<AgentStateSnapshot, OrchestrationError> {
        self.fetch_state_with(StateInclude::default(), limit, 0)
    }

    /// 按需快照（spec §9 fetch_agent_state `include` 选择器 + `limit`/`offset`）。
    pub fn fetch_state_with(
        &self,
        include: StateInclude,
        limit: u32,
        offset: u32,
    ) -> Result<AgentStateSnapshot, OrchestrationError> {
        let active_strategy = if include.strategy { self.strategy.active()? } else { None };
        let recent_runs = if include.runs {
            self.runtime_repo.list_recent_runs_paged(limit, offset)?
        } else {
            Vec::new()
        };
        let recent_results = if include.analysis_results {
            self.runtime_repo.list_recent_analysis_results_paged(limit, offset)?
        } else {
            Vec::new()
        };
        let recent_trades = if include.trades {
            self.runtime_repo.list_recent_trades(limit, offset)?
        } else {
            Vec::new()
        };
        if include.messages || include.tool_calls {
            tracing::warn!(
                target: "runtime.fetch_state",
                messages = include.messages,
                tool_calls = include.tool_calls,
                "fetch_agent_state include requested without global read model (returns empty)"
            );
        }
        Ok(AgentStateSnapshot {
            active_strategy,
            recent_runs,
            recent_results,
            recent_trades,
            recent_messages: Vec::new(),
            recent_tool_calls: Vec::new(),
        })
    }
}
