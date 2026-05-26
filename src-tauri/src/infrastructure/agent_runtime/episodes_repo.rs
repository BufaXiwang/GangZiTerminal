//! decision_episodes 持久化 —— spec `agent-runtime-module.md §2`。
//! `insert` / `update_action_status` / `validate_for_operate` 已接入
//! record_decision_episode + operate_account 流程；`get` 保留作为 audit 单 row 查询。

use crate::infrastructure::db::helpers::now;
use crate::infrastructure::db::{migrate, open_database};
use crate::domain::agent_runtime::decisions::{
    DecisionEpisode, EpisodeAction, EpisodeActionStatus,
};
use rusqlite::{params, Connection};
use tauri::AppHandle;

fn conn(app: &AppHandle) -> Result<Connection, String> {
    let c = open_database(app)?;
    migrate(&c)?;
    Ok(c)
}

fn action_str(a: EpisodeAction) -> &'static str {
    match a {
        EpisodeAction::NoAction => "no_action",
        EpisodeAction::AddWatchlist => "add_watchlist",
        EpisodeAction::RemoveWatchlist => "remove_watchlist",
        EpisodeAction::PlaceOrder => "place_order",
        EpisodeAction::CancelOrder => "cancel_order",
        EpisodeAction::OpenPosition => "open_position",
        EpisodeAction::ScalePosition => "scale_position",
        EpisodeAction::ClosePosition => "close_position",
        EpisodeAction::AdjustProtection => "adjust_protection",
        EpisodeAction::RecordInvalidationSignal => "record_invalidation_signal",
    }
}

fn status_str(s: EpisodeActionStatus) -> &'static str {
    match s {
        EpisodeActionStatus::NoAction => "no_action",
        EpisodeActionStatus::Intended => "intended",
        EpisodeActionStatus::Submitted => "submitted",
        EpisodeActionStatus::Blocked => "blocked",
        EpisodeActionStatus::Deferred => "deferred",
    }
}

pub fn insert(app: &AppHandle, ep: &DecisionEpisode) -> Result<(), String> {
    let c = conn(app)?;
    let symbols =
        serde_json::to_string(&ep.symbols).map_err(|e| format!("symbols 序列化失败：{e}"))?;
    let risk_plan = ep
        .risk_plan
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| format!("risk_plan 序列化失败：{e}"))?;
    let strategy_ids = serde_json::to_string(&ep.strategy_ids)
        .map_err(|e| format!("strategy_ids 序列化失败：{e}"))?;
    let evidence = serde_json::to_string(&ep.evidence_refs)
        .map_err(|e| format!("evidence 序列化失败：{e}"))?;
    c.execute(
        "insert into decision_episodes(
            episode_id, run_id, trigger_kind, symbols_json, thesis,
            action, action_status, blocked_reason, confidence, risk_plan_json,
            strategy_ids_json, evidence_refs_json, created_at, updated_at
         ) values (?1,?2,?3,?4,?5, ?6,?7,?8,?9,?10, ?11,?12,?13,?13)",
        params![
            ep.episode_id,
            ep.run_id,
            ep.trigger_kind,
            symbols,
            ep.thesis,
            action_str(ep.action),
            status_str(ep.action_status),
            ep.blocked_reason,
            ep.confidence,
            risk_plan,
            strategy_ids,
            evidence,
            ep.created_at,
        ],
    )
    .map_err(|e| format!("写 decision_episode 失败：{e}"))?;
    Ok(())
}

pub fn update_action_status(
    app: &AppHandle,
    episode_id: &str,
    status: EpisodeActionStatus,
    blocked_reason: Option<&str>,
) -> Result<(), String> {
    let c = conn(app)?;
    c.execute(
        "update decision_episodes
         set action_status = ?2,
             blocked_reason = coalesce(?3, blocked_reason),
             updated_at = ?4
         where episode_id = ?1",
        params![episode_id, status_str(status), blocked_reason, now()],
    )
    .map_err(|e| format!("update episode 失败：{e}"))?;
    Ok(())
}

/// 校验：episode 必须存在、属于当前 run、actionStatus ∈ {intended, submitted}，且
/// action ∈ 交易类动作。用于 spec §4「operate_account.episodeId 必须指向同一 run
/// 中已接受的 DecisionEpisode」。
pub fn validate_for_operate(
    app: &AppHandle,
    episode_id: &str,
    run_id: &str,
) -> Result<(), String> {
    let c = conn(app)?;
    let row: Option<(String, String, String)> = c
        .query_row(
            "select run_id, action_status, action from decision_episodes where episode_id = ?1",
            params![episode_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
        )
        .ok();
    match row {
        None => Err(format!("not_found: episode {episode_id} 不存在")),
        Some((ep_run, _, _)) if ep_run != run_id => Err(format!(
            "invalid_input: episode {episode_id} 属于 run {ep_run}，与当前 run {run_id} 不一致"
        )),
        Some((_, status, _)) if status != "intended" && status != "submitted" => Err(format!(
            "invalid_input: episode {episode_id} actionStatus = `{status}`，须为 intended/submitted"
        )),
        Some((_, _, action)) if !is_trading_action(&action) => Err(format!(
            "invalid_input: episode {episode_id} action = `{action}` 不是交易类动作，\
             不能用于 operate_account（spec agent-runtime-module.md §2 / §4）"
        )),
        Some(_) => Ok(()),
    }
}

fn is_trading_action(s: &str) -> bool {
    matches!(
        s,
        "place_order"
            | "cancel_order"
            | "open_position"
            | "scale_position"
            | "close_position"
            | "adjust_protection"
            | "record_invalidation_signal"
    )
}

pub fn list_recent(
    app: &AppHandle,
    limit: i64,
) -> Result<Vec<serde_json::Value>, String> {
    let c = conn(app)?;
    let mut stmt = c
        .prepare(
            "select episode_id, run_id, trigger_kind, symbols_json, thesis,
                    action, action_status, blocked_reason, confidence, risk_plan_json,
                    strategy_ids_json, evidence_refs_json, created_at
             from decision_episodes order by created_at desc limit ?1",
        )
        .map_err(|e| format!("prepare 失败：{e}"))?;
    let mut rows = stmt
        .query(params![limit])
        .map_err(|e| format!("query 失败：{e}"))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(|e| format!("next 失败：{e}"))? {
        out.push(row_to_json(r)?);
    }
    Ok(out)
}

fn row_to_json(r: &rusqlite::Row<'_>) -> Result<serde_json::Value, String> {
    let json_or_null = |s: Option<String>| -> serde_json::Value {
        s.and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(serde_json::Value::Null)
    };
    let json_or_array = |s: String| -> serde_json::Value {
        serde_json::from_str(&s).unwrap_or(serde_json::Value::Array(vec![]))
    };
    Ok(serde_json::json!({
        "episodeId": r.get::<_, String>(0).map_err(|e| e.to_string())?,
        "runId": r.get::<_, String>(1).map_err(|e| e.to_string())?,
        "triggerKind": r.get::<_, String>(2).map_err(|e| e.to_string())?,
        "symbols": json_or_array(r.get::<_, String>(3).map_err(|e| e.to_string())?),
        "thesis": r.get::<_, String>(4).map_err(|e| e.to_string())?,
        "action": r.get::<_, String>(5).map_err(|e| e.to_string())?,
        "actionStatus": r.get::<_, String>(6).map_err(|e| e.to_string())?,
        "blockedReason": r.get::<_, Option<String>>(7).map_err(|e| e.to_string())?,
        "confidence": r.get::<_, Option<f64>>(8).map_err(|e| e.to_string())?,
        "riskPlan": json_or_null(r.get::<_, Option<String>>(9).map_err(|e| e.to_string())?),
        "strategyIds": json_or_array(r.get::<_, String>(10).map_err(|e| e.to_string())?),
        "evidenceRefs": json_or_array(r.get::<_, String>(11).map_err(|e| e.to_string())?),
        "createdAt": r.get::<_, String>(12).map_err(|e| e.to_string())?,
    }))
}
