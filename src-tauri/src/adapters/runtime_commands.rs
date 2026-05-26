//! Agent Runtime Tauri commands —— spec `agent-runtime-module.md §9`。
//!
//! - `send_agent_message`：用户消息入口（backbone 复用 chat::send_chat_message_now）
//! - `fetch_agent_state`：聚合读 agent_runs / decision_episodes / decision_reviews / strategy_cards
//! - `cancel_agent_run`：协作式取消 —— set cancel flag + 把 DB status 推到 cancelled；
//!   running run 在下个 turn 入口自然退出，已提交 tool 跑完不被中断（spec §9）。

#![allow(dead_code)] // FetchAgentStateInclude/Request 持有 spec 全字段（messages/toolCalls/providerStatus 等前端 include 由 UI 按需启用）

use crate::adapters::agent_tools::build_full_registry;
use crate::infrastructure::agent_runtime::{
    episodes_repo, reviews_repo, runs_repo, strategy_cards_repo,
};
use crate::infrastructure::db::helpers::now;
use crate::domain::agent_runtime::runs::AgentRunStatus;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tauri::AppHandle;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendAgentMessageRequest {
    pub content: String,
    #[serde(default)]
    pub images: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendAgentMessageResponse {
    pub message_id: String,
    pub run_id: String,
}

#[tauri::command]
pub async fn send_agent_message(
    app: AppHandle,
    request: SendAgentMessageRequest,
) -> Result<SendAgentMessageResponse, String> {
    let registry = Arc::new(build_full_registry(&app));
    let r = crate::pipeline::chat::send_chat_message_now(
        app,
        request.content,
        request.images,
        registry,
    )
    .await?;
    Ok(SendAgentMessageResponse {
        message_id: r.user_message_id,
        run_id: r.run_id,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchAgentStateInclude {
    #[serde(default)]
    pub messages: bool,
    #[serde(default)]
    pub runs: bool,
    #[serde(default)]
    pub episodes: bool,
    #[serde(default)]
    pub reviews: bool,
    #[serde(default)]
    pub strategies: bool,
    #[serde(default)]
    pub tool_calls: bool,
    #[serde(default)]
    pub provider_status: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FetchAgentStateRequest {
    #[serde(default)]
    pub include: Option<FetchAgentStateInclude>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FetchAgentStateResponse {
    pub runs: Vec<Value>,
    pub episodes: Vec<Value>,
    pub reviews: Vec<Value>,
    pub strategies: Vec<Value>,
}

#[tauri::command]
pub fn fetch_agent_state(
    app: AppHandle,
    request: Option<FetchAgentStateRequest>,
) -> Result<FetchAgentStateResponse, String> {
    let req = request.unwrap_or_default();
    let inc = req.include.unwrap_or(FetchAgentStateInclude {
        messages: false,
        runs: true,
        episodes: true,
        reviews: true,
        strategies: true,
        tool_calls: false,
        provider_status: false,
    });
    let limit = req.limit.unwrap_or(50).clamp(1, 500);
    let mut resp = FetchAgentStateResponse::default();
    if inc.runs {
        resp.runs = runs_repo::list_recent(&app, limit)?;
    }
    if inc.episodes {
        resp.episodes = episodes_repo::list_recent(&app, limit)?;
    }
    if inc.reviews {
        resp.reviews = reviews_repo::list_recent(&app, limit)?;
    }
    if inc.strategies {
        let cards = strategy_cards_repo::list(&app, None)?;
        resp.strategies = cards
            .into_iter()
            .filter_map(|c| serde_json::to_value(c).ok())
            .collect();
    }
    Ok(resp)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelAgentRunRequest {
    pub run_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelAgentRunResponse {
    pub accepted: bool,
    pub run_id: String,
    pub status: String,
}

/// spec §9 response.status union 严格 4 值 `cancelled | completed | failed | not_found`。
/// queued / running 不在响应集合内 —— clamp 成 "cancelled"（取消请求受理；终态尚未到达
/// 时按受理处理，前端通过 agent-run-finished 等终态事件）。
fn status_label(s: AgentRunStatus) -> String {
    match s {
        AgentRunStatus::Completed => "completed".into(),
        AgentRunStatus::Failed => "failed".into(),
        AgentRunStatus::Cancelled => "cancelled".into(),
        // queued / running 在 cancel 响应中视为 cancelled（已受理）
        AgentRunStatus::Queued | AgentRunStatus::Running => "cancelled".into(),
    }
}

#[tauri::command]
pub fn cancel_agent_run(
    app: AppHandle,
    request: CancelAgentRunRequest,
) -> Result<CancelAgentRunResponse, String> {
    let CancelAgentRunRequest { run_id, reason } = request;
    let reason_str = reason.unwrap_or_else(|| "user_cancelled".to_string());
    let current = runs_repo::get_status(&app, &run_id)?;
    // spec §9 response.status ∈ cancelled | completed | failed | not_found
    let (accepted, final_status) = match current {
        None => (false, "not_found".to_string()),
        Some(AgentRunStatus::Completed) => (false, "completed".to_string()),
        Some(AgentRunStatus::Failed) => (false, "failed".to_string()),
        Some(AgentRunStatus::Cancelled) => (true, "cancelled".to_string()),
        Some(AgentRunStatus::Queued) => {
            let n = runs_repo::update_status(
                &app,
                &run_id,
                AgentRunStatus::Cancelled,
                Some(&reason_str),
                Some(&now()),
            )?;
            if n > 0 {
                (true, "cancelled".to_string())
            } else {
                // 终态保护拒了 → 回读真实 status
                let real = runs_repo::get_status(&app, &run_id)?
                    .map(status_label)
                    .unwrap_or_else(|| "not_found".into());
                (real == "cancelled", real)
            }
        }
        Some(AgentRunStatus::Running) => {
            // 协作式取消：spec §9「已提交 tool call 的 run 不强中断」+
            // 「取消成功后 AgentRun.status = "cancelled"」。
            // → set flag 立即把 DB 推到 cancelled（让 status 返回值严格落在
            //    spec 4 值 union）；loop 在下个 turn 入口自然退出后由
            //    background_run 写一次 cancelled 状态（幂等覆盖相同终态）。
            let was_running =
                crate::infrastructure::agent_runtime::cancellation::cancel(&run_id);
            if was_running {
                tracing::info!(%run_id, "cancel_agent_run: cooperative flag set");
            }
            let n = runs_repo::update_status(
                &app,
                &run_id,
                AgentRunStatus::Cancelled,
                Some(&reason_str),
                Some(&now()),
            )?;
            if n > 0 {
                (true, "cancelled".to_string())
            } else {
                // SQL guard 拒了（已经是 completed/failed）→ 回读真实状态
                let real = runs_repo::get_status(&app, &run_id)?
                    .map(status_label)
                    .unwrap_or_else(|| "not_found".into());
                (real == "cancelled", real)
            }
        }
    };
    Ok(CancelAgentRunResponse {
        accepted,
        run_id,
        status: final_status,
    })
}
