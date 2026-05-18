//! Tauri IPC——agent_episodes 时间线查询（Today 页主屏数据源）。

use crate::infrastructure::db::{migrate, open_database};
use serde::Serialize;
use serde_json::{json, Value};
use tauri::AppHandle;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EpisodeDto {
    run_id: String,
    trigger_kind: String,
    trigger_ref: Option<String>,
    started_at: String,
    ended_at: Option<String>,
    turns: u32,
    local_tool_calls: u32,
    stop_reason: Option<String>,
    error: Option<String>,
    thesis_ids: Vec<String>,
    outcome_summary: Option<String>,
    parent_episode_id: Option<String>,
}

#[tauri::command]
pub async fn list_agent_episodes(
    app: AppHandle,
    limit: Option<i64>,
) -> Result<Value, String> {
    let limit = limit.unwrap_or(50);
    let conn = open_database(&app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare(
            "select run_id, trigger_kind, trigger_ref, started_at, ended_at,
                    turns, local_tool_calls, stop_reason, error,
                    thesis_ids, outcome_summary, parent_episode_id
             from agent_episodes
             order by started_at desc limit ?1",
        )
        .map_err(|err| format!("准备查询失败：{err}"))?;
    let rows: Vec<EpisodeDto> = stmt
        .query_map(rusqlite::params![limit], |row| {
            let thesis_ids_json: Option<String> = row.get(9)?;
            let thesis_ids: Vec<String> = thesis_ids_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            Ok(EpisodeDto {
                run_id: row.get(0)?,
                trigger_kind: row.get(1)?,
                trigger_ref: row.get(2)?,
                started_at: row.get(3)?,
                ended_at: row.get(4)?,
                turns: row.get(5)?,
                local_tool_calls: row.get(6)?,
                stop_reason: row.get(7)?,
                error: row.get(8)?,
                thesis_ids,
                outcome_summary: row.get(10)?,
                parent_episode_id: row.get(11)?,
            })
        })
        .map_err(|err| format!("查询失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect 失败：{err}"))?;
    Ok(json!(rows))
}

/// 账户主指标（agent-redesign.md § 5.5）—— Today / Positions 页用。
#[tauri::command]
pub async fn get_account_metrics(app: AppHandle) -> Result<Value, String> {
    use crate::infrastructure::account::metrics;
    use crate::infrastructure::account::{snapshot_cache, PositionRepo};
    let positions = PositionRepo::new(app.clone()).list_all().unwrap_or_default();
    let snap = snapshot_cache::get();
    let (total_assets, realized, unrealized) = match snap {
        Some(s) => (
            s.total_assets.value(),
            s.realized_pnl.value(),
            s.unrealized_pnl.value(),
        ),
        None => (
            crate::infrastructure::account::valuation::INITIAL_CASH,
            0.0,
            0.0,
        ),
    };
    let m = metrics::compute(&app, &positions, total_assets, realized, unrealized)?;
    Ok(serde_json::to_value(m).map_err(|e| format!("序列化失败：{e}"))?)
}
