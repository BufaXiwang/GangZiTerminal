//! Agent 健康度指标（精简版）。
//!
//! 当前只暴露 scheduler heartbeat 投影。完整的 AgentRun / DecisionEpisode /
//! TradeIntent 健康度通过 `fetch_agent_state` 命令读取。

use crate::infrastructure::scheduler_heartbeat::{list_heartbeats, HeartbeatRow};
use serde::Serialize;
use tauri::AppHandle;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthMetrics {
    pub heartbeats: Vec<HeartbeatRow>,
}

pub fn compute(app: &AppHandle) -> Result<HealthMetrics, String> {
    let heartbeats = list_heartbeats(app).unwrap_or_default();
    Ok(HealthMetrics { heartbeats })
}
