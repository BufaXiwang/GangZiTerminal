//! Profile → ToolRegistry wiring policy。spec `agent-runtime-module.md §4`。
//!
//! `AgentToolName` 是 canonical local tool name union；`allowed_tools` /
//! `allow_trading_write` 是 profile → tool 集合 / 交易写权限的纯策略。
//! 由 `adapters::agent_tools::build_registry_for_profile` 消费做 profile 过滤。

use serde::{Deserialize, Serialize};

use super::runs::AgentRunProfileId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentToolName {
    FetchQuotes,
    FetchNews,
    FetchAccount,
    OperateAccount,
    UpdateWatchlist,
    RecordDecisionEpisode,
    RecordDecisionReview,
}

/// spec §2 默认 profile 的 allowedTools。
pub fn allowed_tools(profile: AgentRunProfileId) -> Vec<AgentToolName> {
    use AgentToolName::*;
    match profile {
        AgentRunProfileId::UserChat
        | AgentRunProfileId::NewsAnalysis
        | AgentRunProfileId::AccountTriggerResponse
        | AgentRunProfileId::ScheduledReview => vec![
            FetchQuotes,
            FetchNews,
            FetchAccount,
            UpdateWatchlist,
            RecordDecisionEpisode,
            RecordDecisionReview,
            OperateAccount,
        ],
        AgentRunProfileId::ManualReplay => vec![
            FetchAccount,
            FetchQuotes,
            FetchNews,
            RecordDecisionReview,
        ],
    }
}

pub fn allow_trading_write(profile: AgentRunProfileId) -> bool {
    !matches!(
        profile,
        AgentRunProfileId::ManualReplay | AgentRunProfileId::ScheduledReview
    )
}
