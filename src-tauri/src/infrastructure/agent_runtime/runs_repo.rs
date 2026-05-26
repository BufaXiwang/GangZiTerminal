//! agent_runs 持久化。

use crate::infrastructure::db::helpers::now;
use crate::infrastructure::db::{migrate, open_database};
use crate::domain::agent_runtime::runs::{
    AgentRun, AgentRunProfileId, AgentRunStatus, AgentRunTrigger,
};
use rusqlite::{params, Connection};
use tauri::AppHandle;

fn conn(app: &AppHandle) -> Result<Connection, String> {
    let c = open_database(app)?;
    migrate(&c)?;
    Ok(c)
}

fn profile_str(p: AgentRunProfileId) -> &'static str {
    match p {
        AgentRunProfileId::UserChat => "user_chat",
        AgentRunProfileId::NewsAnalysis => "news_analysis",
        AgentRunProfileId::AccountTriggerResponse => "account_trigger_response",
        AgentRunProfileId::ScheduledReview => "scheduled_review",
        AgentRunProfileId::ManualReplay => "manual_replay",
    }
}

fn status_str(s: AgentRunStatus) -> &'static str {
    match s {
        AgentRunStatus::Queued => "queued",
        AgentRunStatus::Running => "running",
        AgentRunStatus::Completed => "completed",
        AgentRunStatus::Failed => "failed",
        AgentRunStatus::Cancelled => "cancelled",
    }
}

fn trigger_kind_str(t: &AgentRunTrigger) -> &'static str {
    match t {
        AgentRunTrigger::UserChat { .. } => "user_chat",
        AgentRunTrigger::NewsBatch { .. } => "news_batch",
        AgentRunTrigger::AccountTrigger { .. } => "account_trigger",
        AgentRunTrigger::ScheduledReview { .. } => "scheduled_review",
        AgentRunTrigger::ManualReplay { .. } => "manual_replay",
    }
}

pub fn insert(app: &AppHandle, run: &AgentRun) -> Result<(), String> {
    let c = conn(app)?;
    let trigger_payload =
        serde_json::to_string(&run.trigger).map_err(|e| format!("trigger 序列化失败：{e}"))?;
    c.execute(
        "insert into agent_runs(
            run_id, profile_id, trigger_kind, trigger_payload_json,
            provider, wire_format, model, status,
            started_at, ended_at, error, created_at, updated_at
         ) values (?1,?2,?3,?4, ?5,?6,?7,?8, ?9,?10,?11, ?12,?12)",
        params![
            run.run_id,
            profile_str(run.profile_id),
            trigger_kind_str(&run.trigger),
            trigger_payload,
            run.provider,
            run.wire_format,
            run.model,
            status_str(run.status),
            run.started_at,
            run.ended_at,
            run.error,
            now(),
        ],
    )
    .map_err(|e| format!("写 agent_run 失败：{e}"))?;
    Ok(())
}

/// 返回 SQL 实际 affected 行数 —— spec §9 终态保护：cancelled / completed / failed
/// 不能被覆盖；调用方在 0 行时应回读真实状态，避免响应漂移。
pub fn update_status(
    app: &AppHandle,
    run_id: &str,
    status: AgentRunStatus,
    error: Option<&str>,
    ended_at: Option<&str>,
) -> Result<u64, String> {
    let c = conn(app)?;
    let new_str = status_str(status);
    let n = c.execute(
        "update agent_runs
         set status = ?2, error = coalesce(?3, error),
             ended_at = coalesce(?4, ended_at), updated_at = ?5
         where run_id = ?1
           and (status not in ('cancelled','completed','failed')
                or status = ?2)",
        params![run_id, new_str, error, ended_at, now()],
    )
    .map_err(|e| format!("update agent_run 失败：{e}"))?;
    Ok(n as u64)
}

pub fn list_recent(app: &AppHandle, limit: i64) -> Result<Vec<serde_json::Value>, String> {
    let c = conn(app)?;
    let mut stmt = c
        .prepare(
            "select run_id, profile_id, trigger_kind, trigger_payload_json, provider, wire_format,
                    model, status, started_at, ended_at, error, created_at, updated_at
             from agent_runs order by created_at desc limit ?1",
        )
        .map_err(|e| format!("prepare 失败：{e}"))?;
    let rows = stmt
        .query_map(params![limit], |r| {
            Ok(serde_json::json!({
                "runId": r.get::<_, String>(0)?,
                "profileId": r.get::<_, String>(1)?,
                "triggerKind": r.get::<_, String>(2)?,
                "triggerPayload": serde_json::from_str::<serde_json::Value>(&r.get::<_, String>(3)?).unwrap_or(serde_json::Value::Null),
                "provider": r.get::<_, String>(4)?,
                "wireFormat": r.get::<_, String>(5)?,
                "model": r.get::<_, String>(6)?,
                "status": r.get::<_, String>(7)?,
                "startedAt": r.get::<_, Option<String>>(8)?,
                "endedAt": r.get::<_, Option<String>>(9)?,
                "error": r.get::<_, Option<String>>(10)?,
                "createdAt": r.get::<_, String>(11)?,
                "updatedAt": r.get::<_, String>(12)?,
            }))
        })
        .map_err(|e| format!("query 失败：{e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("row 解析失败：{e}"))?);
    }
    Ok(out)
}

pub fn get_status(app: &AppHandle, run_id: &str) -> Result<Option<AgentRunStatus>, String> {
    let c = conn(app)?;
    let s: Option<String> = c
        .query_row(
            "select status from agent_runs where run_id = ?1",
            params![run_id],
            |r| r.get(0),
        )
        .ok();
    Ok(s.and_then(|x| match x.as_str() {
        "queued" => Some(AgentRunStatus::Queued),
        "running" => Some(AgentRunStatus::Running),
        "completed" => Some(AgentRunStatus::Completed),
        "failed" => Some(AgentRunStatus::Failed),
        "cancelled" => Some(AgentRunStatus::Cancelled),
        _ => None,
    }))
}

/// 启动恢复：把 status=running 的 run 标记为 failed("interrupted_by_restart")。
pub fn recover_interrupted(app: &AppHandle) -> Result<u64, String> {
    let c = conn(app)?;
    let n = c
        .execute(
            "update agent_runs
             set status = 'failed',
                 error = coalesce(error, 'interrupted_by_restart'),
                 ended_at = coalesce(ended_at, ?1),
                 updated_at = ?1
             where status = 'running'",
            params![now()],
        )
        .map_err(|e| format!("recover_interrupted 失败：{e}"))?;
    Ok(n as u64)
}
