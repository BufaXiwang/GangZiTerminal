//! LLM 本地工具的具体实现 + 组装入口（chat registry）。
//!
//! 对齐 docs/design/agent-runtime-module.md `AgentToolName`：
//! - `fetch_quotes` / `fetch_news` / `fetch_account`：只读
//! - `operate_account`：交易写（trading_write）
//! - `update_watchlist`：非交易写
//! - `record_decision_episode` / `record_decision_review`：Agent Runtime 审计写
//!
//! 抽象（`Tool` trait / `ToolRegistry` / `ToolContext`）在 [`crate::pipeline::agent::tools`]——
//! pipeline 只依赖那层抽象。
//!
//! 具体工具的 input/output schema 直接走对应模块 canonical request；adapter 不再做内部业务判断。

use std::sync::Arc;
use tauri::AppHandle;

use crate::domain::agent_runtime::runs::AgentRunProfileId;
use crate::domain::agent_runtime::tools::{allow_trading_write, allowed_tools, AgentToolName};
use crate::pipeline::agent::tools::ToolRegistry;

pub mod account;
pub mod compact_now;
pub mod decisions;
pub mod news;
pub mod quotes;

/// 按 profile 的 `allowedTools` 过滤构造工具集 —— spec §4 验收点
/// 「Runtime 决定本次 run 注册哪些工具；Infra 只执行注册表内工具」。
///
/// - `operate_account` 仅在 profile 的 `allowedTools` 含它 **且** `allow_trading_write = true`
///   时才注册（spec §2「operate_account 只有在 allowTradingWrite = true 且 allowedTools 包含
///   它时才可暴露」）。
/// - `allow_trading_write_override`：传 `Some(true)/Some(false)` 覆盖默认；
///   主要用于 `scheduled_review` 走 KV 配置开关。其它 profile 通常传 None。
pub fn build_registry_for_profile(
    app: &AppHandle,
    profile: AgentRunProfileId,
    allow_trading_write_override: Option<bool>,
) -> ToolRegistry {
    let allowed: std::collections::HashSet<AgentToolName> =
        allowed_tools(profile).into_iter().collect();
    let trading_write = allow_trading_write_override.unwrap_or_else(|| allow_trading_write(profile));

    let mut reg = ToolRegistry::new();
    // 候选工具集 —— 按 profile.allowedTools 过滤 + Tool::side_effect()
    // 走 ToolRegistry::allow_register 守门（spec §2「sideEffect = trading_write 必须由
    // Runtime 显式允许」）。
    let candidates: Vec<(AgentToolName, Arc<dyn crate::pipeline::agent::tools::Tool>)> = vec![
        (AgentToolName::FetchQuotes, Arc::new(quotes::FetchQuotesTool::new(app.clone()))),
        (AgentToolName::FetchNews, Arc::new(news::FetchNewsTool::new(app.clone()))),
        (AgentToolName::FetchAccount, Arc::new(account::FetchAccountTool::new(app.clone()))),
        (AgentToolName::UpdateWatchlist, Arc::new(account::UpdateWatchlistTool::new(app.clone()))),
        (AgentToolName::RecordDecisionEpisode, Arc::new(decisions::RecordDecisionEpisodeTool::new(app.clone()))),
        (AgentToolName::RecordDecisionReview, Arc::new(decisions::RecordDecisionReviewTool::new(app.clone()))),
        (AgentToolName::OperateAccount, Arc::new(account::OperateAccountTool::new(app.clone()))),
        (AgentToolName::CompactNow, Arc::new(compact_now::CompactNowTool::new())),
    ];
    for (kind, tool) in candidates {
        if !allowed.contains(&kind) {
            continue;
        }
        // spec §2 enforce：trading_write 工具必须 allow_trading_write 才允许注册
        if !crate::pipeline::agent::tools::ToolRegistry::allow_register(&tool, trading_write) {
            continue;
        }
        reg.register(tool);
    }
    reg
}

/// 向后兼容入口：用户 chat run（spec `user_chat` profile）注册全量 7 tool。
pub fn build_full_registry(app: &AppHandle) -> ToolRegistry {
    build_registry_for_profile(app, AgentRunProfileId::UserChat, None)
}

fn parse_profile(s: &str) -> AgentRunProfileId {
    match s {
        "news_analysis" => AgentRunProfileId::NewsAnalysis,
        "account_trigger_response" => AgentRunProfileId::AccountTriggerResponse,
        "scheduled_review" => AgentRunProfileId::ScheduledReview,
        "manual_replay" => AgentRunProfileId::ManualReplay,
        _ => AgentRunProfileId::UserChat,
    }
}

/// 给 `pipeline::agent::tools::install_registry_factory` 用的入口。
pub fn registry_factory(
    app: &AppHandle,
    profile_id: &str,
    allow_trading_write_override: Option<bool>,
) -> ToolRegistry {
    build_registry_for_profile(app, parse_profile(profile_id), allow_trading_write_override)
}
