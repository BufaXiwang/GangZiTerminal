//! Tauri IPC——Thesis aggregate 只读查询（前端 ThesesPage / Positions 详情用）。
//!
//! 写操作（create / update_state / attach_feedback）只通过 agent tool 调，
//! 前端永远没有"创建 thesis"按钮（per agent-redesign.md § 14 用户只观察不操作）。

use crate::domain::account::thesis::{Thesis, ThesisId, ThesisState};
use crate::infrastructure::account::thesis_repo;
use serde::Serialize;
use serde_json::{json, Value};
use tauri::AppHandle;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThesisDto {
    id: String,
    hypothesis: String,
    invalidation: String,
    validation_checks: Vec<String>,
    conviction: String,
    state: String,
    target_codes: Vec<String>,
    regime_at_creation: Option<String>,
    created_at: i64,
    updated_at: i64,
    closed_at: Option<i64>,
}

impl From<&Thesis> for ThesisDto {
    fn from(t: &Thesis) -> Self {
        Self {
            id: t.id.as_str().to_string(),
            hypothesis: t.hypothesis.clone(),
            invalidation: t.invalidation.clone(),
            validation_checks: t.validation_checks.clone(),
            conviction: t.conviction.as_str().to_string(),
            state: t.state.as_str().to_string(),
            target_codes: t.target_codes.iter().map(|c| c.as_str().to_string()).collect(),
            regime_at_creation: t.regime_at_creation.as_ref().map(|r| r.as_str().to_string()),
            created_at: t.created_at.value(),
            updated_at: t.updated_at.value(),
            closed_at: t.closed_at.as_ref().map(|o| o.value()),
        }
    }
}

#[tauri::command]
pub async fn list_theses(
    app: AppHandle,
    filter: Option<String>,
    limit: Option<i64>,
) -> Result<Value, String> {
    let limit = limit.unwrap_or(100);
    let theses = match filter.as_deref() {
        Some("open") | None => thesis_repo::list_open_theses(&app, limit)?,
        Some(state) => {
            let parsed = match state {
                "drafted" => ThesisState::Drafted,
                "active" => ThesisState::Active,
                "validated" => ThesisState::Validated,
                "drifted" => ThesisState::Drifted,
                "invalidated" => ThesisState::Invalidated,
                "abandoned" => ThesisState::Abandoned,
                other => return Err(format!("非法 state filter: {other}")),
            };
            thesis_repo::list_theses_by_state(&app, parsed, limit)?
        }
    };
    let dtos: Vec<ThesisDto> = theses.iter().map(ThesisDto::from).collect();
    Ok(json!(dtos))
}

#[tauri::command]
pub async fn get_thesis(app: AppHandle, thesis_id: String) -> Result<Option<Value>, String> {
    let id = ThesisId::from_string(thesis_id);
    let t = thesis_repo::get_thesis(&app, &id)?;
    Ok(t.as_ref().map(|t| serde_json::to_value(ThesisDto::from(t)).unwrap()))
}

#[tauri::command]
pub async fn list_thesis_events(app: AppHandle, thesis_id: String) -> Result<Value, String> {
    let id = ThesisId::from_string(thesis_id);
    let events = thesis_repo::list_thesis_events(&app, &id)?;
    let dtos: Vec<Value> = events
        .into_iter()
        .map(|rec| {
            json!({
                "thesisId": rec.thesis_id.as_str(),
                "event": rec.event,
                "occurredAt": rec.occurred_at.value(),
            })
        })
        .collect();
    Ok(json!(dtos))
}
