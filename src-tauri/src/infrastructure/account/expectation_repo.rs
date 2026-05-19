//! Expectation aggregate 的 SQLite 持久化（归属 account BC）。
//!
//! 见 docs/design/agent-v3-expectation-driven.md § 2.1。
//!
//! 三张表：
//! - `expectations`：主表
//! - `expectation_events`：状态机审计 + 用户反馈 append-only
//! - `simulated_positions.current_expectation_id`：跨表引用（持仓关联）

use crate::domain::account::expectation::{
    Direction, Expectation, ExpectationEvent, ExpectationEventRecord, ExpectationId,
    ExpectationState,
};
use crate::domain::account::expectation::Conviction;
use crate::domain::shared::signal::SignalKind;
use crate::domain::quotes::regime::Regime;
use crate::domain::shared::{OccurredAt, StockCode, Yuan};
use crate::infrastructure::db::{migrate, open_database};
use rusqlite::{params, OptionalExtension};
use tauri::{AppHandle, Emitter};

pub const EVENT_EXPECTATIONS_CHANGED: &str = "expectations-changed";

fn emit_changed(app: &AppHandle) {
    let _ = app.emit(EVENT_EXPECTATIONS_CHANGED, serde_json::json!({}));
}

// ====== 写入 ============================================================

/// 创建一个 expectation——一个事务里同时写 expectations 行 + created 事件。
pub fn create(app: &AppHandle, exp: &Expectation) -> Result<(), String> {
    let mut conn = open_database(app)?;
    migrate(&conn)?;
    let tx = conn
        .transaction()
        .map_err(|err| format!("开启事务失败：{err}"))?;
    let signals_json = serde_json::to_string(&exp.signals_used)
        .map_err(|err| format!("序列化 signals_used 失败：{err}"))?;
    tx.execute(
        "insert into expectations
            (id, code, direction, target_price, target_price_ceiling, horizon_days,
             reasoning, signals_used, conviction, theme, supersedes_expectation_id,
             state, regime_at_creation, created_at, expires_at, closed_at)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            exp.id.as_str(),
            exp.code.as_str(),
            exp.direction.as_str(),
            exp.target_price.as_ref().map(|y| y.value()),
            exp.target_price_ceiling.as_ref().map(|y| y.value()),
            exp.horizon_days as i64,
            exp.reasoning,
            signals_json,
            exp.conviction.as_str(),
            exp.theme,
            exp.supersedes.as_ref().map(|i| i.as_str().to_string()),
            exp.state.as_str(),
            exp.regime_at_creation.as_ref().map(|r| r.as_str()),
            exp.created_at.to_rfc3339(),
            exp.expires_at.to_rfc3339(),
            exp.closed_at.as_ref().map(|o| o.to_rfc3339()),
        ],
    )
    .map_err(|err| format!("插入 expectation 失败：{err}"))?;
    let payload = serde_json::to_string(&ExpectationEvent::Created)
        .map_err(|err| format!("序列化 expectation_event 失败：{err}"))?;
    tx.execute(
        "insert into expectation_events (expectation_id, kind, payload, occurred_at)
         values (?1, ?2, ?3, ?4)",
        params![
            exp.id.as_str(),
            "created",
            payload,
            exp.created_at.to_rfc3339()
        ],
    )
    .map_err(|err| format!("写 expectation_events(created) 失败：{err}"))?;
    tx.commit()
        .map_err(|err| format!("提交事务失败：{err}"))?;
    emit_changed(app);
    Ok(())
}

/// 推进 state 到终态 + append 对应事件 + 标 closed_at。
pub fn transition(
    app: &AppHandle,
    id: &ExpectationId,
    new_state: ExpectationState,
    event: ExpectationEvent,
    now: OccurredAt,
) -> Result<(), String> {
    if !new_state.is_terminal() {
        return Err(format!(
            "transition 仅用于终态推进；non-terminal {} 请用 append_event",
            new_state.as_str()
        ));
    }
    let mut conn = open_database(app)?;
    migrate(&conn)?;
    let tx = conn
        .transaction()
        .map_err(|err| format!("开启事务失败：{err}"))?;
    tx.execute(
        "update expectations
         set state = ?2,
             closed_at = coalesce(closed_at, ?3)
         where id = ?1",
        params![id.as_str(), new_state.as_str(), now.to_rfc3339()],
    )
    .map_err(|err| format!("更新 expectation state 失败：{err}"))?;
    let payload = serde_json::to_string(&event)
        .map_err(|err| format!("序列化 expectation_event 失败：{err}"))?;
    tx.execute(
        "insert into expectation_events (expectation_id, kind, payload, occurred_at)
         values (?1, ?2, ?3, ?4)",
        params![id.as_str(), event.kind_str(), payload, now.to_rfc3339()],
    )
    .map_err(|err| format!("写 expectation_events 失败：{err}"))?;
    tx.commit()
        .map_err(|err| format!("提交事务失败：{err}"))?;
    emit_changed(app);
    Ok(())
}

/// append 非终态事件（user_feedback / note 等），不动 state。
pub fn append_event(
    app: &AppHandle,
    id: &ExpectationId,
    event: ExpectationEvent,
    now: OccurredAt,
) -> Result<(), String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let payload = serde_json::to_string(&event)
        .map_err(|err| format!("序列化 expectation_event 失败：{err}"))?;
    conn.execute(
        "insert into expectation_events (expectation_id, kind, payload, occurred_at)
         values (?1, ?2, ?3, ?4)",
        params![id.as_str(), event.kind_str(), payload, now.to_rfc3339()],
    )
    .map_err(|err| format!("写 expectation_events 失败：{err}"))?;
    emit_changed(app);
    Ok(())
}

/// 更新 reasoning / target_price / horizon 等可调字段（仅对 pending 状态有效）。
pub fn update_fields(
    app: &AppHandle,
    id: &ExpectationId,
    target_price: Option<Yuan>,
    target_price_ceiling: Option<Yuan>,
    horizon_days: Option<u32>,
    new_expires_at: Option<OccurredAt>,
    reasoning: Option<String>,
) -> Result<(), String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    use rusqlite::types::Value;
    let mut sets: Vec<&'static str> = Vec::new();
    let mut binds: Vec<Value> = Vec::new();
    if let Some(p) = target_price {
        sets.push("target_price = ?");
        binds.push(Value::Real(p.value()));
    }
    if let Some(p) = target_price_ceiling {
        sets.push("target_price_ceiling = ?");
        binds.push(Value::Real(p.value()));
    }
    if let Some(h) = horizon_days {
        sets.push("horizon_days = ?");
        binds.push(Value::Integer(h as i64));
    }
    if let Some(t) = new_expires_at {
        sets.push("expires_at = ?");
        binds.push(Value::Text(t.to_rfc3339()));
    }
    if let Some(r) = reasoning {
        sets.push("reasoning = ?");
        binds.push(Value::Text(r));
    }
    if sets.is_empty() {
        return Ok(());
    }
    let sql = format!(
        "update expectations set {} where id = ? and state = 'pending'",
        sets.join(", ")
    );
    binds.push(Value::Text(id.as_str().to_string()));
    conn.execute(&sql, rusqlite::params_from_iter(binds))
        .map_err(|err| format!("更新 expectation 失败：{err}"))?;
    emit_changed(app);
    Ok(())
}

// ====== 读取 ============================================================

pub fn get(app: &AppHandle, id: &ExpectationId) -> Result<Option<Expectation>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let row = conn
        .query_row(
            "select code, direction, target_price, target_price_ceiling, horizon_days,
                    reasoning, signals_used, conviction, theme, supersedes_expectation_id,
                    state, regime_at_creation, created_at, expires_at, closed_at
             from expectations where id = ?1",
            params![id.as_str()],
            row_to_expectation_with_id(id.clone()),
        )
        .optional()
        .map_err(|err| format!("读取 expectation 失败：{err}"))?;
    Ok(row.transpose()?)
}

pub fn list_pending_for_code(
    app: &AppHandle,
    code: &StockCode,
) -> Result<Vec<Expectation>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare(
            "select id, code, direction, target_price, target_price_ceiling, horizon_days,
                    reasoning, signals_used, conviction, theme, supersedes_expectation_id,
                    state, regime_at_creation, created_at, expires_at, closed_at
             from expectations
             where state = 'pending' and code = ?1
             order by created_at desc",
        )
        .map_err(|err| format!("准备 list_pending_for_code 失败：{err}"))?;
    let rows = stmt
        .query_map(params![code.as_str()], row_to_expectation_full)
        .map_err(|err| format!("query 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect 失败：{err}"))?;
    rows.into_iter().collect::<Result<Vec<_>, _>>()
}

pub fn list_pending(app: &AppHandle, limit: i64) -> Result<Vec<Expectation>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare(
            "select id, code, direction, target_price, target_price_ceiling, horizon_days,
                    reasoning, signals_used, conviction, theme, supersedes_expectation_id,
                    state, regime_at_creation, created_at, expires_at, closed_at
             from expectations
             where state = 'pending'
             order by expires_at asc limit ?1",
        )
        .map_err(|err| format!("准备 list_pending 失败：{err}"))?;
    let rows = stmt
        .query_map(params![limit], row_to_expectation_full)
        .map_err(|err| format!("query 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect 失败：{err}"))?;
    rows.into_iter().collect::<Result<Vec<_>, _>>()
}

pub fn list_by_state(
    app: &AppHandle,
    state: ExpectationState,
    limit: i64,
) -> Result<Vec<Expectation>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare(
            "select id, code, direction, target_price, target_price_ceiling, horizon_days,
                    reasoning, signals_used, conviction, theme, supersedes_expectation_id,
                    state, regime_at_creation, created_at, expires_at, closed_at
             from expectations where state = ?1
             order by created_at desc limit ?2",
        )
        .map_err(|err| format!("准备 list_by_state 失败：{err}"))?;
    let rows = stmt
        .query_map(params![state.as_str(), limit], row_to_expectation_full)
        .map_err(|err| format!("query 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect 失败：{err}"))?;
    rows.into_iter().collect::<Result<Vec<_>, _>>()
}

pub fn list_events(
    app: &AppHandle,
    id: &ExpectationId,
) -> Result<Vec<ExpectationEventRecord>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare(
            "select payload, occurred_at from expectation_events
             where expectation_id = ?1 order by occurred_at, id",
        )
        .map_err(|err| format!("准备 list_events 失败：{err}"))?;
    let rows: Vec<(Option<String>, String)> = stmt
        .query_map(params![id.as_str()], |row| {
            Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|err| format!("query 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect 失败：{err}"))?;
    let mut out = Vec::with_capacity(rows.len());
    for (payload, ts) in rows {
        let event: ExpectationEvent = match payload {
            Some(text) => serde_json::from_str(&text)
                .map_err(|err| format!("反序列化 expectation_event 失败：{err}"))?,
            None => ExpectationEvent::Created,
        };
        out.push(ExpectationEventRecord {
            expectation_id: id.clone(),
            event,
            occurred_at: parse_occurred(&ts)?,
        });
    }
    Ok(out)
}

// ====== row 解析 helper =================================================

fn row_to_expectation_with_id(
    id: ExpectationId,
) -> impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<Result<Expectation, String>> {
    move |row| {
        let code: String = row.get(0)?;
        let direction: String = row.get(1)?;
        let target_price: Option<f64> = row.get(2)?;
        let target_price_ceiling: Option<f64> = row.get(3)?;
        let horizon_days: i64 = row.get(4)?;
        let reasoning: String = row.get(5)?;
        let signals_used: String = row.get(6)?;
        let conviction: String = row.get(7)?;
        let theme: Option<String> = row.get(8)?;
        let supersedes: Option<String> = row.get(9)?;
        let state: String = row.get(10)?;
        let regime: Option<String> = row.get(11)?;
        let created_at: String = row.get(12)?;
        let expires_at: String = row.get(13)?;
        let closed_at: Option<String> = row.get(14)?;
        Ok(build_expectation(
            id.clone(),
            code,
            direction,
            target_price,
            target_price_ceiling,
            horizon_days,
            reasoning,
            signals_used,
            conviction,
            theme,
            supersedes,
            state,
            regime,
            created_at,
            expires_at,
            closed_at,
        ))
    }
}

fn row_to_expectation_full(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<Expectation, String>> {
    let id_str: String = row.get(0)?;
    let id = ExpectationId::from_string(id_str);
    let code: String = row.get(1)?;
    let direction: String = row.get(2)?;
    let target_price: Option<f64> = row.get(3)?;
    let target_price_ceiling: Option<f64> = row.get(4)?;
    let horizon_days: i64 = row.get(5)?;
    let reasoning: String = row.get(6)?;
    let signals_used: String = row.get(7)?;
    let conviction: String = row.get(8)?;
    let theme: Option<String> = row.get(9)?;
    let supersedes: Option<String> = row.get(10)?;
    let state: String = row.get(11)?;
    let regime: Option<String> = row.get(12)?;
    let created_at: String = row.get(13)?;
    let expires_at: String = row.get(14)?;
    let closed_at: Option<String> = row.get(15)?;
    Ok(build_expectation(
        id,
        code,
        direction,
        target_price,
        target_price_ceiling,
        horizon_days,
        reasoning,
        signals_used,
        conviction,
        theme,
        supersedes,
        state,
        regime,
        created_at,
        expires_at,
        closed_at,
    ))
}

#[allow(clippy::too_many_arguments)]
fn build_expectation(
    id: ExpectationId,
    code: String,
    direction: String,
    target_price: Option<f64>,
    target_price_ceiling: Option<f64>,
    horizon_days: i64,
    reasoning: String,
    signals_used: String,
    conviction: String,
    theme: Option<String>,
    supersedes: Option<String>,
    state: String,
    regime: Option<String>,
    created_at: String,
    expires_at: String,
    closed_at: Option<String>,
) -> Result<Expectation, String> {
    let signals: Vec<SignalKind> = serde_json::from_str(&signals_used)
        .map_err(|err| format!("反序列化 signals_used 失败：{err}"))?;
    Ok(Expectation {
        id,
        code: StockCode::new(&code).map_err(|e| format!("非法 code {code}: {e:?}"))?,
        direction: Direction::parse(&direction)
            .ok_or_else(|| format!("未知 direction: {direction}"))?,
        target_price: target_price
            .map(Yuan::from_unchecked),
        target_price_ceiling: target_price_ceiling
            .map(Yuan::from_unchecked),
        horizon_days: horizon_days as u32,
        reasoning,
        signals_used: signals,
        conviction: Conviction::parse(&conviction)
            .ok_or_else(|| format!("未知 conviction: {conviction}"))?,
        theme,
        supersedes: supersedes.map(ExpectationId::from_string),
        state: ExpectationState::parse(&state)
            .ok_or_else(|| format!("未知 expectation state: {state}"))?,
        regime_at_creation: regime.as_deref().and_then(Regime::parse),
        created_at: parse_occurred(&created_at)?,
        expires_at: parse_occurred(&expires_at)?,
        closed_at: match closed_at {
            Some(s) => Some(parse_occurred(&s)?),
            None => None,
        },
    })
}

fn parse_occurred(s: &str) -> Result<OccurredAt, String> {
    let dt = chrono::DateTime::parse_from_rfc3339(s)
        .map_err(|err| format!("解析 RFC3339 失败 ({s}): {err}"))?;
    Ok(OccurredAt::new(dt.timestamp_millis()))
}
