//! Tauri IPC——Expectation / Strategy / Lesson / Heuristic 只读查询（前端 v3 页面用）。

use crate::domain::account::expectation::{Expectation, ExpectationId, ExpectationState};
use crate::infrastructure::account::expectation_repo;
use crate::infrastructure::agent::{heuristic_repo, lesson_repo, strategy_repo};
use serde_json::{json, Value};
use tauri::AppHandle;

#[tauri::command]
pub async fn list_expectations(
    app: AppHandle,
    state: Option<String>,
    limit: Option<i64>,
) -> Result<Value, String> {
    let limit = limit.unwrap_or(200);
    let result: Vec<Expectation> = match state.as_deref() {
        None | Some("pending") => expectation_repo::list_pending(&app, limit)?,
        Some(s) => {
            let parsed = ExpectationState::parse(s)
                .ok_or_else(|| format!("非法 state: {s}"))?;
            expectation_repo::list_by_state(&app, parsed, limit)?
        }
    };
    Ok(serde_json::to_value(result).map_err(|e| format!("序列化失败：{e}"))?)
}

#[tauri::command]
pub async fn get_expectation(
    app: AppHandle,
    expectation_id: String,
) -> Result<Option<Value>, String> {
    let id = ExpectationId::from_string(expectation_id);
    let exp = expectation_repo::get(&app, &id)?;
    Ok(exp.map(|e| serde_json::to_value(e).unwrap()))
}

#[tauri::command]
pub async fn list_expectation_events(
    app: AppHandle,
    expectation_id: String,
) -> Result<Value, String> {
    let id = ExpectationId::from_string(expectation_id);
    let events = expectation_repo::list_events(&app, &id)?;
    Ok(serde_json::to_value(events).map_err(|e| format!("序列化失败：{e}"))?)
}

#[tauri::command]
pub async fn list_strategies(app: AppHandle) -> Result<Value, String> {
    let strats = strategy_repo::list_all(&app)?;
    Ok(serde_json::to_value(strats).map_err(|e| format!("序列化失败：{e}"))?)
}

#[tauri::command]
pub async fn list_lessons(app: AppHandle, limit: Option<i64>) -> Result<Value, String> {
    let lessons = lesson_repo::list_recent(&app, limit.unwrap_or(100))?;
    Ok(serde_json::to_value(lessons).map_err(|e| format!("序列化失败：{e}"))?)
}

#[tauri::command]
pub async fn list_heuristics(app: AppHandle, limit: Option<i64>) -> Result<Value, String> {
    let heuristics = heuristic_repo::list_all(&app, limit.unwrap_or(200))?;
    // 派生 effective_state 给前端用
    let dtos: Vec<Value> = heuristics
        .iter()
        .map(|h| {
            json!({
                "id": h.id.as_str(),
                "body": h.body,
                "category": h.category.as_str(),
                "origin": h.origin.as_str(),
                "regimeTags": h.regime_tags.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
                "supportingLessonIds": h.supporting_lesson_ids.iter().map(|l| l.as_str()).collect::<Vec<_>>(),
                "applicationCount": h.application_count,
                "hitCount": h.hit_count,
                "missCount": h.miss_count,
                "confidence": h.confidence(),
                "effectiveState": h.effective_state().as_str(),
                "lastAppliedAt": h.last_applied_at.as_ref().map(|o| o.value()),
                "retiredAt": h.retired_at.as_ref().map(|o| o.value()),
                "createdAt": h.created_at.value(),
            })
        })
        .collect();
    Ok(json!(dtos))
}

#[tauri::command]
pub async fn get_heuristic_counts(app: AppHandle) -> Result<Value, String> {
    let c = heuristic_repo::count_by_state(&app)?;
    Ok(json!({
        "seed": c.seed,
        "userStated": c.user_stated,
        "agentInferred": c.agent_inferred,
        "retired": c.retired,
    }))
}
