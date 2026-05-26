//! Profile → ToolRegistry wiring policy。spec `agent-runtime-module.md §4`。
//!
//! `AgentToolName` 是 canonical local tool name union；`allowed_tools` /
//! `allow_trading_write` 是 profile → tool 集合 / 交易写权限的纯策略。
//! 由 `adapters::agent_tools::build_registry_for_profile` 消费做 profile 过滤。

use serde::{Deserialize, Serialize};

use super::runs::AgentRunProfileId;

/// spec `agent-runtime-module.md §101`：业务工具闭集合 7 个。
/// 新增业务工具必须先扩 spec，再加 variant。
///
/// 注：`compact_now` 是 Agent Infra 层的"上下文控制"工具（spec agent-infra-module.md §4），
/// 不属于 AgentToolName 业务集合；它由 registry 工厂额外注入（所有 profile 可用），
/// 不通过 profile.allowedTools 过滤。
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

/// spec `agent-runtime-module.md §2`：每个 profile 的 packet 必需 section 集合。
/// Packet builder 必须保证这些 section 在 packet 里非空 / 已注入，否则 fail closed。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PacketSection {
    Account,
    Quotes,
    News,
    Strategies,
    RecentEpisodes,
    UserPreferences,
}

impl PacketSection {
    pub fn as_str(self) -> &'static str {
        match self {
            PacketSection::Account => "account",
            PacketSection::Quotes => "quotes",
            PacketSection::News => "news",
            PacketSection::Strategies => "strategies",
            PacketSection::RecentEpisodes => "recent_episodes",
            PacketSection::UserPreferences => "user_preferences",
        }
    }
}

pub fn required_packet_sections(profile: AgentRunProfileId) -> Vec<PacketSection> {
    use PacketSection::*;
    match profile {
        AgentRunProfileId::UserChat => {
            // 账户 / 行情 / 新闻按工具调用实时读取
            vec![Strategies, RecentEpisodes, UserPreferences]
        }
        AgentRunProfileId::NewsAnalysis
        | AgentRunProfileId::AccountTriggerResponse
        | AgentRunProfileId::ScheduledReview => vec![
            News,
            Quotes,
            Account,
            Strategies,
            RecentEpisodes,
            UserPreferences,
        ],
        AgentRunProfileId::ManualReplay => vec![
            Account,
            Quotes,
            News,
            Strategies,
            RecentEpisodes,
        ],
    }
}
