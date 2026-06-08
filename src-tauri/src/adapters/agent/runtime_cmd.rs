//! Agent Runtime Tauri commands —— 对话 / 状态 / 策略读写。
//!
//! Spec: docs/design/agent-runtime-module.md §9 对外接口
//!
//! 暴露：
//! - `agent_send_message`：dialogue trigger（用户消息 → 连续对话线程 run）。流式 token / tool /
//!   run 起止经 `agent-event` 事件推前端；command 在 run 终态时 resolve，回最终 run。
//! - `agent_fetch_state`：总览快照（active 策略 + 最近 runs + 最近 AnalysisResults）。
//! - `agent_fetch_strategy` / `agent_upsert_strategy`：投资策略读 / 写（写仅经对话用户确认，§3）。
//!
//! `cancel_agent_run` / `list_review_reports` 待取消令牌 / review fork 接线后补（见 §WP2 余 / §WP3）。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State;

use crate::adapters::error::CommandError;
use crate::domain::agent::runtime::{InvestmentStrategy, StrategyStatus};
use crate::domain::shared::ErrorCode;
use crate::pipeline::agent_runtime::orchestrator::{
    AgentStateSnapshot, CancelRunStatus, OrchestrationError, StateInclude,
};
use crate::pipeline::agent_runtime::strategy::StrategyError;
use crate::pipeline::agent_runtime::RuntimeServices;

fn map_orchestration(e: OrchestrationError) -> CommandError {
    match e {
        OrchestrationError::NoActiveChannel => {
            CommandError::with_message(ErrorCode::ProviderUnavailable, e.to_string())
        }
        OrchestrationError::Provider(m) => {
            CommandError::with_message(ErrorCode::ProviderUnavailable, m)
        }
        other => CommandError::with_message(ErrorCode::DbError, other.to_string()),
    }
}

// ---------------------------------------------------------------- dialogue

/// spec §9 `SendAgentMessageRequest = { content; images?; conversationId? }`。
///
/// `conversationId?`（spec §9 补充）：dialogue 是独立连续对话线程（§3），传入续接已有线程；
/// 不传则后端新开一个匿名线程。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageInput {
    /// 用户消息文本（spec `content`）。
    pub content: String,
    /// 多模态附件（base64/URL）；当前 loop 仅消费文本，images 接受但未透传（见模块注）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    /// 续接的对话线程 id（spec §9 补充）；不传则后端新开匿名线程。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
}

/// spec §9 `SendAgentMessageResponse = { messageId; runId }`。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageResult {
    pub message_id: String,
    pub run_id: String,
}

/// 提交一条用户消息，跑一次 dialogue run（同步等到终态；流式经 `agent-event`）。
#[tauri::command]
#[specta::specta]
pub async fn agent_send_message(
    services: State<'_, Arc<RuntimeServices>>,
    input: SendMessageInput,
) -> Result<SendMessageResult, CommandError> {
    if input.content.trim().is_empty() {
        return Err(CommandError::with_message(ErrorCode::InvalidInput, "消息不能为空"));
    }
    let conversation_id = input
        .conversation_id
        .unwrap_or_else(|| format!("conv_{}", uuid::Uuid::new_v4()));
    let images = input.images.unwrap_or_default();
    let res = services
        .run_dialogue_detailed(conversation_id, input.content, images)
        .await
        .map_err(map_orchestration)?;
    Ok(SendMessageResult {
        message_id: res.message_id,
        run_id: res.run.run_id,
    })
}

// ---------------------------------------------------------------- 状态

/// spec §9 fetch_agent_state `include` 选择器（未选的段省略）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FetchStateInclude {
    #[serde(default)]
    pub strategy: Option<bool>,
    #[serde(default)]
    pub runs: Option<bool>,
    #[serde(default)]
    pub analysis_results: Option<bool>,
    #[serde(default)]
    pub trades: Option<bool>,
    #[serde(default)]
    pub messages: Option<bool>,
    #[serde(default)]
    pub tool_calls: Option<bool>,
}

/// spec §9 `FetchAgentStateRequest = { include?; limit?; offset? }`。
#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FetchStateInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<FetchStateInclude>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
}

/// Agent 总览快照（按 `include` 选择器返回各段；未选的省略）。
///
/// 未传 `include` → 默认返回 strategy + runs + analysisResults（向后兼容）。
#[tauri::command]
#[specta::specta]
pub fn agent_fetch_state(
    services: State<'_, Arc<RuntimeServices>>,
    input: Option<FetchStateInput>,
) -> Result<AgentStateSnapshot, CommandError> {
    let input = input.unwrap_or_default();
    let include = match input.include {
        Some(sel) => StateInclude {
            strategy: sel.strategy.unwrap_or(false),
            runs: sel.runs.unwrap_or(false),
            analysis_results: sel.analysis_results.unwrap_or(false),
            trades: sel.trades.unwrap_or(false),
            messages: sel.messages.unwrap_or(false),
            tool_calls: sel.tool_calls.unwrap_or(false),
        },
        None => StateInclude::default(),
    };
    services
        .fetch_state_with(include, input.limit.unwrap_or(50), input.offset.unwrap_or(0))
        .map_err(map_orchestration)
}

// ---------------------------------------------------------------- 投资策略

/// spec §9 `FetchInvestmentStrategyRequest = { status?; includeHistory? }`。
#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FetchStrategyInput {
    /// 过滤：仅当 active 策略的 status 匹配时返回（None = 不过滤）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StrategyStatus>,
    /// true = 附带 active strategyId 的历史版本列表。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_history: Option<bool>,
}

/// spec §9 历史版本条目。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct StrategyHistoryEntry {
    pub version: u32,
    pub updated_at: crate::domain::shared::OccurredAt,
    pub reason: String,
}

/// spec §9 `FetchInvestmentStrategyResponse = { active?; history? }`。
#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FetchStrategyResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<InvestmentStrategy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<StrategyHistoryEntry>>,
}

/// 当前 active 投资策略（+ 可选历史版本，+ status 过滤）。
#[tauri::command]
#[specta::specta]
pub fn agent_fetch_strategy(
    services: State<'_, Arc<RuntimeServices>>,
    input: Option<FetchStrategyInput>,
) -> Result<FetchStrategyResult, CommandError> {
    let input = input.unwrap_or_default();
    let active = services
        .strategy
        .active()
        .map_err(|e| CommandError::with_message(ErrorCode::DbError, e.to_string()))?;
    // status 过滤：active 策略 status 不匹配则视为「无 active」。
    let active = match (active, input.status) {
        (Some(s), Some(want)) if s.status != want => None,
        (a, _) => a,
    };
    let history = if input.include_history.unwrap_or(false) {
        match active.as_ref() {
            Some(s) => {
                let versions = services
                    .strategy
                    .list_versions(&s.strategy_id)
                    .map_err(|e| CommandError::with_message(ErrorCode::DbError, e.to_string()))?;
                Some(
                    versions
                        .into_iter()
                        .map(|(version, updated_at, reason)| StrategyHistoryEntry {
                            version,
                            updated_at,
                            reason: reason.unwrap_or_default(),
                        })
                        .collect(),
                )
            }
            None => Some(Vec::new()),
        }
    } else {
        None
    };
    Ok(FetchStrategyResult { active, history })
}

/// spec §9 `UpsertInvestmentStrategyRequest = { strategyId?; baseVersion?; strategy; status; reason }`。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct UpsertStrategyInput {
    /// 目标策略 id；不传 = 更新当前 active 策略的 id（无 active 时用 baseline id）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy_id: Option<String>,
    /// 乐观并发基线版本（前端拿当前 active.version 回填）；不传 = 不校验。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<u32>,
    /// 策略自然语言全文。
    pub strategy: String,
    /// 写入后状态（spec：`"active" | "paused"`）。
    pub status: StrategyStatus,
    /// 写入理由（审计）。
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct UpsertStrategyResult {
    pub strategy_id: String,
    pub version: u32,
}

// ---------------------------------------------------------------- 取消

/// spec §9 `CancelAgentRunRequest = { runId; reason? }`。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CancelRunInput {
    pub run_id: String,
    /// 取消理由（审计）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// spec §9 `CancelAgentRunResponse.status`。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum CancelRunStatusDto {
    Cancelled,
    Completed,
    Failed,
    NotFound,
}

/// spec §9 `CancelAgentRunResponse = { accepted; runId; status }`。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CancelRunResult {
    pub accepted: bool,
    pub run_id: String,
    pub status: CancelRunStatusDto,
}

/// 取消一个在跑的 run（dialogue/news/review 等）。
#[tauri::command]
#[specta::specta]
pub fn agent_cancel_run(
    services: State<'_, Arc<RuntimeServices>>,
    input: CancelRunInput,
) -> Result<CancelRunResult, CommandError> {
    if let Some(reason) = input.reason.as_deref() {
        tracing::info!(target: "runtime.cancel", run_id = %input.run_id, reason, "cancel_agent_run");
    }
    let outcome = services.cancel_run_detailed(&input.run_id);
    let status = match outcome.status {
        CancelRunStatus::Cancelled => CancelRunStatusDto::Cancelled,
        CancelRunStatus::Completed => CancelRunStatusDto::Completed,
        CancelRunStatus::Failed => CancelRunStatusDto::Failed,
        CancelRunStatus::NotFound => CancelRunStatusDto::NotFound,
    };
    Ok(CancelRunResult {
        accepted: outcome.accepted,
        run_id: input.run_id,
        status,
    })
}

// ---------------------------------------------------------------- 复盘

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RunReviewInput {
    /// 复盘交易日（YYYYMMDD 或 YYYY-MM-DD）。
    pub trade_date: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RunReviewResult {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_path: Option<String>,
}

/// 手动触发一次收盘复盘（只读 review run → 落盘 markdown 报告）。流式经 `agent-event`。
#[tauri::command]
#[specta::specta]
pub async fn agent_run_review(
    services: State<'_, Arc<RuntimeServices>>,
    input: RunReviewInput,
) -> Result<RunReviewResult, CommandError> {
    let date = crate::domain::shared::TradeDate::parse(&input.trade_date)
        .map_err(|e| CommandError::with_message(ErrorCode::InvalidInput, e.to_string()))?;
    let r = services.run_eod_review(date).await.map_err(map_orchestration)?;
    Ok(RunReviewResult {
        run_id: r.run.run_id,
        report_path: r.report_path,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ReviewReportRef {
    pub name: String,
    pub path: String,
}

/// 列出已落盘的复盘报告（最新在前）。`limit` 截断返回条数（spec §9 `ListReviewReportsRequest`）。
#[tauri::command]
#[specta::specta]
pub fn agent_list_review_reports(
    services: State<'_, Arc<RuntimeServices>>,
    limit: Option<u32>,
) -> Result<Vec<ReviewReportRef>, CommandError> {
    let mut reports: Vec<ReviewReportRef> = services
        .list_review_reports()
        .into_iter()
        .map(|(name, path)| ReviewReportRef { name, path })
        .collect();
    if let Some(n) = limit {
        let n = n as usize;
        if reports.len() > n {
            tracing::debug!(target: "runtime.review", total = reports.len(), limit = n, "list_review_reports truncated");
            reports.truncate(n);
        }
    }
    Ok(reports)
}

// ---------------------------------------------------------------- news 自动分析开关

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SetNewsAutoAnalysisResult {
    pub enabled: bool,
    /// false→true 开启时回填最近窗口内 news 入 buffer 的条数（spec §5）。
    pub backfilled: u32,
}

/// 读 news 自动分析开关当前状态（spec §5：默认关闭）。
#[tauri::command]
#[specta::specta]
pub fn agent_get_news_auto_analysis(
    services: State<'_, Arc<RuntimeServices>>,
) -> Result<bool, CommandError> {
    Ok(services.news_auto_analysis_enabled())
}

/// 开/关 news 自动分析（spec §5）。开启（false→true）时回填最近 `news_buffer_window_secs`
/// 内的 news 入 buffer。返回新状态 + 回填条数。
#[tauri::command]
#[specta::specta]
pub async fn agent_set_news_auto_analysis(
    services: State<'_, Arc<RuntimeServices>>,
    enabled: bool,
) -> Result<SetNewsAutoAnalysisResult, CommandError> {
    let backfilled = services
        .set_news_auto_analysis_enabled(enabled)
        .await
        .map_err(|e| CommandError::with_message(ErrorCode::DbError, e.to_string()))?;
    Ok(SetNewsAutoAnalysisResult {
        enabled,
        backfilled: backfilled as u32,
    })
}

// ---------------------------------------------------------------- 对话 / tool_call 加载

/// 加载指定对话的全部持久化消息（按 seq 排序；审计真源）。
///
/// 加载对话消息（从最近 summary 检查点开始，不加载更早的历史）。
/// Spec: agent-infra-module.md §5 `load_conversation_view`
#[tauri::command]
#[specta::specta]
pub fn agent_load_conversation(
    infra: State<'_, crate::infrastructure::agent::AgentInfra>,
    conversation_id: String,
) -> Result<Vec<crate::domain::agent::AgentMessage>, CommandError> {
    infra
        .repo
        .load_conversation_view(&conversation_id)
        .map_err(|e| CommandError::with_message(ErrorCode::DbError, e.to_string()))
}

/// 加载指定 run 的全部 ToolCall（按 started_at 升序）。
///
/// Spec: agent-infra-module.md §5
#[tauri::command]
#[specta::specta]
pub fn agent_load_tool_calls(
    infra: State<'_, crate::infrastructure::agent::AgentInfra>,
    run_id: String,
) -> Result<Vec<crate::domain::agent::ToolCall>, CommandError> {
    infra
        .repo
        .load_tool_calls_by_run(&run_id)
        .map_err(|e| CommandError::with_message(ErrorCode::DbError, e.to_string()))
}

/// 读取复盘报告文件内容（Markdown）。
#[tauri::command]
#[specta::specta]
pub fn agent_read_review_report(path: String) -> Result<String, CommandError> {
    std::fs::read_to_string(&path)
        .map_err(|e| CommandError::with_message(ErrorCode::NotFound, format!("读取报告失败: {e}")))
}

/// 写一个策略新版本（用户在对话中确认后调；版本化 + 乐观并发）。
#[tauri::command]
#[specta::specta]
pub fn agent_upsert_strategy(
    services: State<'_, Arc<RuntimeServices>>,
    input: UpsertStrategyInput,
) -> Result<UpsertStrategyResult, CommandError> {
    if input.strategy.trim().is_empty() {
        return Err(CommandError::with_message(ErrorCode::InvalidInput, "策略文本不能为空"));
    }
    match services.strategy.upsert(
        input.strategy_id.as_deref(),
        input.base_version,
        input.strategy,
        input.status,
        &input.reason,
    ) {
        Ok((strategy_id, version)) => Ok(UpsertStrategyResult { strategy_id, version }),
        Err(StrategyError::VersionConflict { expected, actual }) => Err(CommandError::with_message(
            ErrorCode::VersionConflict,
            format!("策略版本冲突：期望基线 v{expected}，当前最新 {actual:?}"),
        )),
        Err(StrategyError::Db(e)) => {
            Err(CommandError::with_message(ErrorCode::DbError, e.to_string()))
        }
    }
}
