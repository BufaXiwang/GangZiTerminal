//! Thesis aggregate 的 SQLite 持久化（归属 account BC）。
//!
//! 三张表：
//! - `theses`：主表，存核心字段
//! - `thesis_codes`：thesis ↔ 股票多对多（一个 thesis 可对应多只股票）
//! - `thesis_events`：状态机转换 + 用户反馈 append-only
//!
//! API 风格与 PositionRepo 对齐：
//! - 写操作：create / update_state / append_event
//! - 读操作：get / list_by_state / list_for_code / list_events

use crate::domain::account::thesis::{
    Conviction, Thesis, ThesisEvent, ThesisEventRecord, ThesisId, ThesisState,
};
use crate::domain::quotes::regime::Regime;
use crate::domain::shared::{OccurredAt, StockCode};
use crate::infrastructure::db::{migrate, now, open_database};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use tauri::AppHandle;

// ====== 写入 ============================================================

/// 创建一个新的 thesis（带 thesis_codes 关联表）+ 写 created 事件。
/// 一个事务里完成。
pub fn create_thesis(app: &AppHandle, thesis: &Thesis) -> Result<(), String> {
    let mut conn = open_database(app)?;
    migrate(&conn)?;
    let tx = conn
        .transaction()
        .map_err(|err| format!("开启事务失败：{err}"))?;
    let validation_json = serde_json::to_string(&thesis.validation_checks)
        .map_err(|err| format!("序列化 validation_checks 失败：{err}"))?;
    tx.execute(
        "insert into theses
            (id, hypothesis, invalidation, validation_checks, conviction, state,
             regime_at_creation, created_at, updated_at, closed_at)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            thesis.id.as_str(),
            thesis.hypothesis,
            thesis.invalidation,
            validation_json,
            thesis.conviction.as_str(),
            thesis.state.as_str(),
            thesis.regime_at_creation.as_ref().map(|r| r.as_str()),
            thesis.created_at.to_rfc3339(),
            thesis.updated_at.to_rfc3339(),
            thesis.closed_at.as_ref().map(|o| o.to_rfc3339()),
        ],
    )
    .map_err(|err| format!("插入 thesis 失败：{err}"))?;
    for code in &thesis.target_codes {
        tx.execute(
            "insert or ignore into thesis_codes (thesis_id, code) values (?1, ?2)",
            params![thesis.id.as_str(), code.as_str()],
        )
        .map_err(|err| format!("插入 thesis_codes 失败：{err}"))?;
    }
    // 写 created 事件
    let initial_event = if matches!(thesis.state, ThesisState::Active) {
        // active 状态直接落 created + activated 两条
        Some(ThesisEvent::Activated)
    } else {
        None
    };
    let create_payload = serde_json::to_string(&ThesisEvent::Created)
        .map_err(|err| format!("序列化 thesis_event 失败：{err}"))?;
    tx.execute(
        "insert into thesis_events (thesis_id, kind, payload, occurred_at)
         values (?1, ?2, ?3, ?4)",
        params![
            thesis.id.as_str(),
            "created",
            create_payload,
            thesis.created_at.to_rfc3339()
        ],
    )
    .map_err(|err| format!("写 thesis_events(created) 失败：{err}"))?;
    if let Some(activated) = initial_event {
        let activated_payload = serde_json::to_string(&activated)
            .map_err(|err| format!("序列化 thesis_event 失败：{err}"))?;
        tx.execute(
            "insert into thesis_events (thesis_id, kind, payload, occurred_at)
             values (?1, ?2, ?3, ?4)",
            params![
                thesis.id.as_str(),
                "activated",
                activated_payload,
                thesis.created_at.to_rfc3339()
            ],
        )
        .map_err(|err| format!("写 thesis_events(activated) 失败：{err}"))?;
    }
    tx.commit()
        .map_err(|err| format!("提交事务失败：{err}"))?;
    Ok(())
}

/// 更新 thesis state（带 closed_at 自动落）+ append 对应事件。
pub fn update_thesis_state(
    app: &AppHandle,
    id: &ThesisId,
    new_state: ThesisState,
    event: ThesisEvent,
    now_occurred: OccurredAt,
) -> Result<(), String> {
    let mut conn = open_database(app)?;
    migrate(&conn)?;
    let tx = conn
        .transaction()
        .map_err(|err| format!("开启事务失败：{err}"))?;
    let closed_at = if new_state.is_terminal() {
        Some(now_occurred.to_rfc3339())
    } else {
        None
    };
    tx.execute(
        "update theses set state = ?2, updated_at = ?3,
                closed_at = coalesce(?4, closed_at)
         where id = ?1",
        params![id.as_str(), new_state.as_str(), now_occurred.to_rfc3339(), closed_at],
    )
    .map_err(|err| format!("更新 thesis state 失败：{err}"))?;
    let payload = serde_json::to_string(&event)
        .map_err(|err| format!("序列化 thesis_event 失败：{err}"))?;
    tx.execute(
        "insert into thesis_events (thesis_id, kind, payload, occurred_at)
         values (?1, ?2, ?3, ?4)",
        params![id.as_str(), event.kind_str(), payload, now_occurred.to_rfc3339()],
    )
    .map_err(|err| format!("写 thesis_events 失败：{err}"))?;
    tx.commit()
        .map_err(|err| format!("提交事务失败：{err}"))?;
    Ok(())
}

/// 追加一条 thesis 事件（不改 state）——用于 ValidationCheckHit / UserFeedback 等。
pub fn append_thesis_event(
    app: &AppHandle,
    id: &ThesisId,
    event: ThesisEvent,
    occurred_at: OccurredAt,
) -> Result<(), String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let payload = serde_json::to_string(&event)
        .map_err(|err| format!("序列化 thesis_event 失败：{err}"))?;
    conn.execute(
        "insert into thesis_events (thesis_id, kind, payload, occurred_at)
         values (?1, ?2, ?3, ?4)",
        params![id.as_str(), event.kind_str(), payload, occurred_at.to_rfc3339()],
    )
    .map_err(|err| format!("写 thesis_events 失败：{err}"))?;
    // 触碰 updated_at
    conn.execute(
        "update theses set updated_at = ?2 where id = ?1",
        params![id.as_str(), occurred_at.to_rfc3339()],
    )
    .map_err(|err| format!("更新 theses.updated_at 失败：{err}"))?;
    Ok(())
}

// ====== 读取 ============================================================

pub fn get_thesis(app: &AppHandle, id: &ThesisId) -> Result<Option<Thesis>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let row = conn
        .query_row(
            "select hypothesis, invalidation, validation_checks, conviction, state,
                    regime_at_creation, created_at, updated_at, closed_at
             from theses where id = ?1",
            params![id.as_str()],
            |row| {
                Ok(ThesisRow {
                    hypothesis: row.get(0)?,
                    invalidation: row.get(1)?,
                    validation_checks: row.get(2)?,
                    conviction: row.get(3)?,
                    state: row.get(4)?,
                    regime_at_creation: row.get(5)?,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                    closed_at: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(|err| format!("读取 thesis 失败：{err}"))?;
    let Some(row) = row else { return Ok(None) };
    let codes = list_codes_for_thesis(&conn, id)?;
    Ok(Some(row.into_domain(id.clone(), codes)?))
}

pub fn list_theses_by_state(
    app: &AppHandle,
    state: ThesisState,
    limit: i64,
) -> Result<Vec<Thesis>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare(
            "select id, hypothesis, invalidation, validation_checks, conviction, state,
                    regime_at_creation, created_at, updated_at, closed_at
             from theses where state = ?1 order by updated_at desc limit ?2",
        )
        .map_err(|err| format!("准备 list_theses_by_state 失败：{err}"))?;
    let rows: Vec<(String, ThesisRow)> = stmt
        .query_map(params![state.as_str(), limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                ThesisRow {
                    hypothesis: row.get(1)?,
                    invalidation: row.get(2)?,
                    validation_checks: row.get(3)?,
                    conviction: row.get(4)?,
                    state: row.get(5)?,
                    regime_at_creation: row.get(6)?,
                    created_at: row.get(7)?,
                    updated_at: row.get(8)?,
                    closed_at: row.get(9)?,
                },
            ))
        })
        .map_err(|err| format!("query list_theses_by_state 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect list_theses_by_state 失败：{err}"))?;
    let mut out = Vec::with_capacity(rows.len());
    for (id_str, row) in rows {
        let id = ThesisId::from_string(id_str);
        let codes = list_codes_for_thesis(&conn, &id)?;
        out.push(row.into_domain(id, codes)?);
    }
    Ok(out)
}

pub fn list_open_theses(app: &AppHandle, limit: i64) -> Result<Vec<Thesis>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare(
            "select id, hypothesis, invalidation, validation_checks, conviction, state,
                    regime_at_creation, created_at, updated_at, closed_at
             from theses where state in ('drafted', 'active') order by updated_at desc limit ?1",
        )
        .map_err(|err| format!("准备 list_open_theses 失败：{err}"))?;
    let rows: Vec<(String, ThesisRow)> = stmt
        .query_map(params![limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                ThesisRow {
                    hypothesis: row.get(1)?,
                    invalidation: row.get(2)?,
                    validation_checks: row.get(3)?,
                    conviction: row.get(4)?,
                    state: row.get(5)?,
                    regime_at_creation: row.get(6)?,
                    created_at: row.get(7)?,
                    updated_at: row.get(8)?,
                    closed_at: row.get(9)?,
                },
            ))
        })
        .map_err(|err| format!("query list_open_theses 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect list_open_theses 失败：{err}"))?;
    let mut out = Vec::with_capacity(rows.len());
    for (id_str, row) in rows {
        let id = ThesisId::from_string(id_str);
        let codes = list_codes_for_thesis(&conn, &id)?;
        out.push(row.into_domain(id, codes)?);
    }
    Ok(out)
}

/// 列出包含给定股票的所有 active thesis（agent 决策前查"这只是不是已有论点在跟踪"）。
pub fn list_active_for_code(app: &AppHandle, code: &StockCode) -> Result<Vec<Thesis>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare(
            "select t.id, t.hypothesis, t.invalidation, t.validation_checks, t.conviction,
                    t.state, t.regime_at_creation, t.created_at, t.updated_at, t.closed_at
             from theses t
             inner join thesis_codes c on c.thesis_id = t.id
             where t.state = 'active' and c.code = ?1
             order by t.updated_at desc",
        )
        .map_err(|err| format!("准备 list_active_for_code 失败：{err}"))?;
    let rows: Vec<(String, ThesisRow)> = stmt
        .query_map(params![code.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                ThesisRow {
                    hypothesis: row.get(1)?,
                    invalidation: row.get(2)?,
                    validation_checks: row.get(3)?,
                    conviction: row.get(4)?,
                    state: row.get(5)?,
                    regime_at_creation: row.get(6)?,
                    created_at: row.get(7)?,
                    updated_at: row.get(8)?,
                    closed_at: row.get(9)?,
                },
            ))
        })
        .map_err(|err| format!("query list_active_for_code 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect list_active_for_code 失败：{err}"))?;
    let mut out = Vec::with_capacity(rows.len());
    for (id_str, row) in rows {
        let id = ThesisId::from_string(id_str);
        let codes = list_codes_for_thesis(&conn, &id)?;
        out.push(row.into_domain(id, codes)?);
    }
    Ok(out)
}

pub fn list_thesis_events(
    app: &AppHandle,
    id: &ThesisId,
) -> Result<Vec<ThesisEventRecord>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare(
            "select payload, occurred_at from thesis_events
             where thesis_id = ?1 order by occurred_at, id",
        )
        .map_err(|err| format!("准备 list_thesis_events 失败：{err}"))?;
    let rows: Vec<(Option<String>, String)> = stmt
        .query_map(params![id.as_str()], |row| {
            Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|err| format!("query list_thesis_events 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect list_thesis_events 失败：{err}"))?;
    let mut out = Vec::with_capacity(rows.len());
    for (payload, occurred_at_str) in rows {
        let event: ThesisEvent = match payload {
            Some(text) => serde_json::from_str(&text)
                .map_err(|err| format!("反序列化 thesis_event 失败：{err}"))?,
            None => ThesisEvent::Created,
        };
        out.push(ThesisEventRecord {
            thesis_id: id.clone(),
            event,
            occurred_at: parse_occurred(&occurred_at_str)?,
        });
    }
    Ok(out)
}

// ====== 内部 helper =====================================================

struct ThesisRow {
    hypothesis: String,
    invalidation: String,
    validation_checks: Option<String>,
    conviction: String,
    state: String,
    regime_at_creation: Option<String>,
    created_at: String,
    updated_at: String,
    closed_at: Option<String>,
}

impl ThesisRow {
    fn into_domain(self, id: ThesisId, codes: Vec<StockCode>) -> Result<Thesis, String> {
        let validation_checks: Vec<String> = match self.validation_checks {
            Some(json) => serde_json::from_str(&json)
                .map_err(|err| format!("反序列化 validation_checks 失败：{err}"))?,
            None => Vec::new(),
        };
        let conviction = match self.conviction.as_str() {
            "low" => Conviction::Low,
            "medium" => Conviction::Medium,
            "high" => Conviction::High,
            other => return Err(format!("未知 conviction: {other}")),
        };
        let state = match self.state.as_str() {
            "drafted" => ThesisState::Drafted,
            "active" => ThesisState::Active,
            "validated" => ThesisState::Validated,
            "drifted" => ThesisState::Drifted,
            "invalidated" => ThesisState::Invalidated,
            "abandoned" => ThesisState::Abandoned,
            other => return Err(format!("未知 thesis state: {other}")),
        };
        let regime = self
            .regime_at_creation
            .as_deref()
            .and_then(Regime::parse);
        Ok(Thesis {
            id,
            hypothesis: self.hypothesis,
            invalidation: self.invalidation,
            validation_checks,
            conviction,
            state,
            target_codes: codes,
            regime_at_creation: regime,
            created_at: parse_occurred(&self.created_at)?,
            updated_at: parse_occurred(&self.updated_at)?,
            closed_at: match self.closed_at {
                Some(s) => Some(parse_occurred(&s)?),
                None => None,
            },
        })
    }
}

fn list_codes_for_thesis(
    conn: &rusqlite::Connection,
    id: &ThesisId,
) -> Result<Vec<StockCode>, String> {
    let mut stmt = conn
        .prepare("select code from thesis_codes where thesis_id = ?1")
        .map_err(|err| format!("准备 list_codes_for_thesis 失败：{err}"))?;
    let codes: Vec<String> = stmt
        .query_map(params![id.as_str()], |row| row.get(0))
        .map_err(|err| format!("query thesis_codes 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect thesis_codes 失败：{err}"))?;
    codes
        .into_iter()
        .map(|s| StockCode::new(&s).map_err(|err| format!("无效 StockCode {s}: {err:?}")))
        .collect()
}

fn parse_occurred(s: &str) -> Result<OccurredAt, String> {
    let dt = chrono::DateTime::parse_from_rfc3339(s)
        .map_err(|err| format!("解析 RFC3339 失败 ({s}): {err}"))?;
    Ok(OccurredAt::new(dt.timestamp_millis()))
}

#[allow(dead_code)]
fn _payload_string(v: &Value) -> Option<String> {
    serde_json::to_string(v).ok()
}
