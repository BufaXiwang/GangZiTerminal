//! Signal detection 持久化——每个 tick 跑完写一批，审计 + 命中率统计用。

use crate::domain::shared::signal::SignalKind;
use crate::domain::shared::OccurredAt;
use crate::infrastructure::db::{migrate, open_database};
use rusqlite::params;
use tauri::AppHandle;

pub fn record_batch(
    app: &AppHandle,
    tick_id: &str,
    detections: &[(String, SignalKind, OccurredAt)],
) -> Result<usize, String> {
    if detections.is_empty() {
        return Ok(0);
    }
    let mut conn = open_database(app)?;
    migrate(&conn)?;
    let tx = conn
        .transaction()
        .map_err(|err| format!("开启事务失败：{err}"))?;
    let mut count = 0;
    for (code, signal, ts) in detections {
        let signal_json = serde_json::to_string(signal)
            .map_err(|err| format!("序列化 signal 失败：{err}"))?;
        tx.execute(
            "insert into signal_detections
                (tick_id, code, signal_family, signal_json, detected_at)
             values (?1, ?2, ?3, ?4, ?5)",
            params![
                tick_id,
                code,
                signal.family_str(),
                signal_json,
                ts.to_rfc3339()
            ],
        )
        .map_err(|err| format!("写 signal_detection 失败：{err}"))?;
        count += 1;
    }
    tx.commit()
        .map_err(|err| format!("提交事务失败：{err}"))?;
    Ok(count)
}

pub fn list_for_tick(
    app: &AppHandle,
    tick_id: &str,
) -> Result<Vec<(String, SignalKind, OccurredAt)>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare(
            "select code, signal_json, detected_at from signal_detections
             where tick_id = ?1 order by code, detected_at",
        )
        .map_err(|err| format!("准备 list_for_tick 失败：{err}"))?;
    let rows: Vec<(String, String, String)> = stmt
        .query_map(params![tick_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|err| format!("query 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect 失败：{err}"))?;
    let mut out = Vec::with_capacity(rows.len());
    for (code, json, ts) in rows {
        let signal: SignalKind = serde_json::from_str(&json)
            .map_err(|err| format!("反序列化 signal 失败：{err}"))?;
        out.push((code, signal, parse_occurred(&ts)?));
    }
    Ok(out)
}

/// 列出某 code 自 `since` 之后的所有 detection（升序）——
/// expectation review 时拿来匹配 invalidation_signals 用。
pub fn list_for_code_since(
    app: &AppHandle,
    code: &str,
    since: OccurredAt,
) -> Result<Vec<(SignalKind, OccurredAt)>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare(
            "select signal_json, detected_at from signal_detections
             where code = ?1 and detected_at >= ?2 order by detected_at",
        )
        .map_err(|err| format!("准备 list_for_code_since 失败：{err}"))?;
    let rows: Vec<(String, String)> = stmt
        .query_map(params![code, since.to_rfc3339()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|err| format!("query 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect 失败：{err}"))?;
    let mut out = Vec::with_capacity(rows.len());
    for (json, ts) in rows {
        let signal: SignalKind = serde_json::from_str(&json)
            .map_err(|err| format!("反序列化 signal 失败：{err}"))?;
        out.push((signal, parse_occurred(&ts)?));
    }
    Ok(out)
}

/// 给定 signal family 在最近 N 天的触发次数 + 命中率 — 用于 health metrics。
pub fn signal_family_stats(
    app: &AppHandle,
    days: u32,
) -> Result<Vec<(String, u32)>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
    let mut stmt = conn
        .prepare(
            "select signal_family, count(*) from signal_detections
             where detected_at >= ?1
             group by signal_family
             order by count(*) desc",
        )
        .map_err(|err| format!("准备 signal_family_stats 失败：{err}"))?;
    let rows = stmt
        .query_map(params![cutoff], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
        })
        .map_err(|err| format!("query 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect 失败：{err}"))?;
    Ok(rows)
}

fn parse_occurred(s: &str) -> Result<OccurredAt, String> {
    let dt = chrono::DateTime::parse_from_rfc3339(s)
        .map_err(|err| format!("解析 RFC3339 失败 ({s}): {err}"))?;
    Ok(OccurredAt::new(dt.timestamp_millis()))
}
