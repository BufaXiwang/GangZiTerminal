//! Tauri IPC——Principle aggregate 只读查询 + 健康度统计 + 立即触发 reflection。
//!
//! 写操作（propose / confirm / retire）只通过 agent tool 调，前端永远没有按钮。

use crate::domain::agent::principle::{Principle, PrincipleState};
use crate::infrastructure::agent::{health_metrics, principle_repo};
use crate::adapters::agent_tools::build_chat_registry;
use crate::pipeline::agent::reflect::run_close_reflection;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tauri::AppHandle;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PrincipleDto {
    id: String,
    body: String,
    category: String,
    origin: String,
    state: String,
    regime_tags: Vec<String>,
    hit_count: u32,
    last_applied_at: Option<i64>,
    created_at: i64,
}

impl From<&Principle> for PrincipleDto {
    fn from(p: &Principle) -> Self {
        Self {
            id: p.id.as_str().to_string(),
            body: p.body.clone(),
            category: p.category.as_str().to_string(),
            origin: p.origin.as_str().to_string(),
            state: p.state.as_str().to_string(),
            regime_tags: p.regime_tags.iter().map(|r| r.as_str().to_string()).collect(),
            hit_count: p.hit_count,
            last_applied_at: p.last_applied_at.as_ref().map(|o| o.value()),
            created_at: p.created_at.value(),
        }
    }
}

#[tauri::command]
pub async fn list_principles(
    app: AppHandle,
    state: Option<String>,
    limit: Option<i64>,
) -> Result<Value, String> {
    let limit = limit.unwrap_or(200);
    let principles = match state.as_deref() {
        None => principle_repo::list_all(&app, limit)?,
        Some(s) => {
            let parsed = PrincipleState::parse(s)
                .ok_or_else(|| format!("非法 state: {s}"))?;
            principle_repo::list_by_state(&app, parsed, limit)?
        }
    };
    let dtos: Vec<PrincipleDto> = principles.iter().map(PrincipleDto::from).collect();
    Ok(json!(dtos))
}

#[tauri::command]
pub async fn get_health_metrics(app: AppHandle) -> Result<Value, String> {
    let metrics = health_metrics::compute(&app)?;
    Ok(serde_json::to_value(metrics).map_err(|e| format!("序列化 health metrics 失败：{e}"))?)
}

/// Settings 页"立即跑一次 reflection"按钮。
#[tauri::command]
pub async fn trigger_reflection_now(app: AppHandle) -> Result<Value, String> {
    let registry = Arc::new(build_chat_registry(&app));
    let result = run_close_reflection(app, registry).await?;
    Ok(json!({
        "runId": result.run_id,
        "outcomeSummary": result.outcome_summary,
        "thesisCount": result.thesis_count,
    }))
}
