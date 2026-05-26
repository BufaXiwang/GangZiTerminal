//! decision_reviews 持久化。

use crate::infrastructure::db::{migrate, open_database};
use crate::domain::agent_runtime::decisions::{DecisionReview, DecisionReviewTrigger};
use rusqlite::{params, Connection};
use tauri::AppHandle;

fn conn(app: &AppHandle) -> Result<Connection, String> {
    let c = open_database(app)?;
    migrate(&c)?;
    Ok(c)
}

fn trigger_str(t: DecisionReviewTrigger) -> &'static str {
    match t {
        DecisionReviewTrigger::PositionClosed => "position_closed",
        DecisionReviewTrigger::StopLoss => "stop_loss",
        DecisionReviewTrigger::TakeProfit => "take_profit",
        DecisionReviewTrigger::TimeStop => "time_stop",
        DecisionReviewTrigger::Invalidated => "invalidated",
        DecisionReviewTrigger::OrderFilled => "order_filled",
        DecisionReviewTrigger::OrderRejected => "order_rejected",
        DecisionReviewTrigger::OrderExpired => "order_expired",
        DecisionReviewTrigger::ScheduledReview => "scheduled_review",
        DecisionReviewTrigger::ManualReview => "manual_review",
    }
}

pub fn insert(app: &AppHandle, review: &DecisionReview) -> Result<(), String> {
    let c = conn(app)?;
    let result_json = review
        .result
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| format!("result 序列化失败：{e}"))?;
    let suggested_json = review
        .suggested_change
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| format!("suggested_change 序列化失败：{e}"))?;
    let evidence = serde_json::to_string(&review.evidence_refs)
        .map_err(|e| format!("evidence 序列化失败：{e}"))?;
    let warnings = if review.warnings.is_empty() {
        None
    } else {
        Some(
            serde_json::to_string(&review.warnings)
                .map_err(|e| format!("warnings 序列化失败：{e}"))?,
        )
    };
    c.execute(
        "insert into decision_reviews(
            review_id, episode_id, trigger, result_json, conclusion,
            suggested_change_json, evidence_refs_json, warnings_json, created_at
         ) values (?1,?2,?3,?4,?5, ?6,?7,?8, ?9)",
        params![
            review.review_id,
            review.episode_id,
            trigger_str(review.trigger),
            result_json,
            review.conclusion,
            suggested_json,
            evidence,
            warnings,
            review.created_at,
        ],
    )
    .map_err(|e| format!("写 decision_review 失败：{e}"))?;
    Ok(())
}

pub fn list_recent(app: &AppHandle, limit: i64) -> Result<Vec<serde_json::Value>, String> {
    let c = conn(app)?;
    let mut stmt = c
        .prepare(
            "select review_id, episode_id, trigger, result_json, conclusion,
                    suggested_change_json, evidence_refs_json, warnings_json, created_at
             from decision_reviews order by created_at desc limit ?1",
        )
        .map_err(|e| format!("prepare 失败：{e}"))?;
    let mut rows = stmt
        .query(params![limit])
        .map_err(|e| format!("query 失败：{e}"))?;
    let mut out = Vec::new();
    let json_or_null = |s: Option<String>| -> serde_json::Value {
        s.and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(serde_json::Value::Null)
    };
    let json_or_array = |s: String| -> serde_json::Value {
        serde_json::from_str(&s).unwrap_or(serde_json::Value::Array(vec![]))
    };
    while let Some(r) = rows.next().map_err(|e| format!("next 失败：{e}"))? {
        out.push(serde_json::json!({
            "reviewId": r.get::<_, String>(0).map_err(|e| e.to_string())?,
            "episodeId": r.get::<_, String>(1).map_err(|e| e.to_string())?,
            "trigger": r.get::<_, String>(2).map_err(|e| e.to_string())?,
            "result": json_or_null(r.get::<_, Option<String>>(3).map_err(|e| e.to_string())?),
            "conclusion": r.get::<_, String>(4).map_err(|e| e.to_string())?,
            "suggestedChange": json_or_null(r.get::<_, Option<String>>(5).map_err(|e| e.to_string())?),
            "evidenceRefs": json_or_array(r.get::<_, String>(6).map_err(|e| e.to_string())?),
            "warnings": json_or_null(r.get::<_, Option<String>>(7).map_err(|e| e.to_string())?),
            "createdAt": r.get::<_, String>(8).map_err(|e| e.to_string())?,
        }));
    }
    Ok(out)
}
