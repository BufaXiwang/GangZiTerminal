//! Lesson 持久化——每个 expectation 终态自动生成。

use crate::domain::account::expectation::ExpectationId;
use crate::domain::agent::lesson::{Lesson, LessonId, LessonOutcome};
use crate::domain::shared::signal::SignalKind;
use crate::domain::quotes::regime::Regime;
use crate::domain::shared::{OccurredAt, StockCode};
use crate::infrastructure::db::{migrate, open_database};
use rusqlite::{params, OptionalExtension};
use tauri::AppHandle;

pub fn create(app: &AppHandle, l: &Lesson) -> Result<(), String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let signals_json = serde_json::to_string(&l.signals_in_play)
        .map_err(|err| format!("序列化 signals_in_play 失败：{err}"))?;
    conn.execute(
        "insert into lessons
            (id, expectation_id, code, observation, takeaway, outcome,
             regime_at_close, signals_in_play, pnl_pct, created_at)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            l.id.as_str(),
            l.expectation_id.as_str(),
            l.code.as_str(),
            l.observation,
            l.takeaway,
            l.outcome.as_str(),
            l.regime_at_close.as_ref().map(|r| r.as_str()),
            signals_json,
            l.pnl_pct,
            l.created_at.to_rfc3339(),
        ],
    )
    .map_err(|err| format!("插入 lesson 失败：{err}"))?;
    Ok(())
}

pub fn list_recent(app: &AppHandle, limit: i64) -> Result<Vec<Lesson>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare(
            "select id, expectation_id, code, observation, takeaway, outcome,
                    regime_at_close, signals_in_play, pnl_pct, created_at
             from lessons order by created_at desc limit ?1",
        )
        .map_err(|err| format!("准备 list_recent 失败：{err}"))?;
    let rows = stmt
        .query_map(params![limit], row_to_lesson)
        .map_err(|err| format!("query 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect 失败：{err}"))?;
    rows.into_iter().collect::<Result<Vec<_>, _>>()
}

pub fn list_for_expectation(
    app: &AppHandle,
    id: &ExpectationId,
) -> Result<Vec<Lesson>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare(
            "select id, expectation_id, code, observation, takeaway, outcome,
                    regime_at_close, signals_in_play, pnl_pct, created_at
             from lessons where expectation_id = ?1 order by created_at",
        )
        .map_err(|err| format!("准备 list_for_expectation 失败：{err}"))?;
    let rows = stmt
        .query_map(params![id.as_str()], row_to_lesson)
        .map_err(|err| format!("query 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect 失败：{err}"))?;
    rows.into_iter().collect::<Result<Vec<_>, _>>()
}

pub fn get(app: &AppHandle, id: &LessonId) -> Result<Option<Lesson>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let row = conn
        .query_row(
            "select id, expectation_id, code, observation, takeaway, outcome,
                    regime_at_close, signals_in_play, pnl_pct, created_at
             from lessons where id = ?1",
            params![id.as_str()],
            row_to_lesson,
        )
        .optional()
        .map_err(|err| format!("读取 lesson 失败：{err}"))?;
    row.transpose()
}

fn row_to_lesson(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Lesson, String>> {
    let id: String = row.get(0)?;
    let expectation_id: String = row.get(1)?;
    let code: String = row.get(2)?;
    let observation: String = row.get(3)?;
    let takeaway: String = row.get(4)?;
    let outcome: String = row.get(5)?;
    let regime: Option<String> = row.get(6)?;
    let signals_json: String = row.get(7)?;
    let pnl_pct: Option<f64> = row.get(8)?;
    let created_at: String = row.get(9)?;
    Ok((|| {
        let signals: Vec<SignalKind> = serde_json::from_str(&signals_json)
            .map_err(|err| format!("反序列化 signals_in_play 失败：{err}"))?;
        Ok(Lesson {
            id: LessonId::from_string(id),
            expectation_id: ExpectationId::from_string(expectation_id),
            code: StockCode::new(&code).map_err(|e| format!("非法 code {code}: {e:?}"))?,
            observation,
            takeaway,
            outcome: LessonOutcome::parse(&outcome)
                .ok_or_else(|| format!("未知 outcome: {outcome}"))?,
            regime_at_close: regime.as_deref().and_then(Regime::parse),
            signals_in_play: signals,
            pnl_pct,
            created_at: parse_occurred(&created_at)?,
        })
    })())
}

fn parse_occurred(s: &str) -> Result<OccurredAt, String> {
    let dt = chrono::DateTime::parse_from_rfc3339(s)
        .map_err(|err| format!("解析 RFC3339 失败 ({s}): {err}"))?;
    Ok(OccurredAt::new(dt.timestamp_millis()))
}
