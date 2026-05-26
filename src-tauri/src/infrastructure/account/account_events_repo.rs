//! `account_events` 表 append + 查询 repo —— spec `account-module.md §2`.
//!
//! 设计：所有写动作（订单创建 / 撤销 / 拒绝 / 成交 / 仓位变更 / 保护条件 / 自选 /
//! 现金冻结释放 / 触发器 / snapshot 重建）都必须先调 [`append`] 写一条
//! `AccountEvent`，再更新派生读模型。append 失败必须让写动作 fail closed。
//!
//! 查询接口供 `fetch_account.include.events` 和 `rebuild_account_snapshot` 使用。

use rusqlite::params;
use tauri::AppHandle;

use crate::domain::account::{AccountActor, AccountEvent, AccountEventType};
use crate::infrastructure::db::{migrate, open_database};

/// append 一条 `AccountEvent`。返回写入的 `event_id`。
pub fn append(app: &AppHandle, event: &AccountEvent) -> Result<String, String> {
    let conn = open_database(app).map_err(|e| format!("db open: {e}"))?;
    migrate(&conn).map_err(|e| format!("migrate: {e}"))?;
    conn.execute(
        "insert into account_events(
            event_id, event_type, order_id, fill_id, position_id, ts_code,
            reason, actor, payload_json, occurred_at
         ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            event.event_id,
            event.event_type.as_str(),
            event.order_id,
            event.fill_id,
            event.position_id,
            event.ts_code,
            event.reason,
            event.actor.as_str(),
            event.payload.to_string(),
            event.occurred_at,
        ],
    )
    .map_err(|e| format!("insert account_event: {e}"))?;
    Ok(event.event_id.clone())
}

/// 取最近 N 条事件，按 occurred_at desc。
pub fn list_recent(app: &AppHandle, limit: usize, offset: usize) -> Result<Vec<AccountEvent>, String> {
    let conn = open_database(app).map_err(|e| format!("db open: {e}"))?;
    migrate(&conn).map_err(|e| format!("migrate: {e}"))?;
    let mut stmt = conn
        .prepare(
            "select event_id, event_type, order_id, fill_id, position_id, ts_code,
                    reason, actor, payload_json, occurred_at
             from account_events
             order by occurred_at desc
             limit ?1 offset ?2",
        )
        .map_err(|e| format!("prepare: {e}"))?;
    let rows = stmt
        .query_map(params![limit as i64, offset as i64], row_to_event)
        .map_err(|e| format!("query: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("row: {e}"))?);
    }
    Ok(out)
}

/// 检查是否已经有 `account_initialized` 事件。
pub fn has_account_initialized(app: &AppHandle) -> Result<bool, String> {
    let conn = open_database(app).map_err(|e| format!("db open: {e}"))?;
    migrate(&conn).map_err(|e| format!("migrate: {e}"))?;
    let n: i64 = conn
        .query_row(
            "select count(*) from account_events where event_type = 'account_initialized'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("query: {e}"))?;
    Ok(n > 0)
}

/// 取某 ts_code 的最近一条 watchlist_added 事件的 occurred_at；
/// 用于 `fetch_account.watchlist` 展示「加入自选时间」。
pub fn latest_watchlist_added_at(
    app: &AppHandle,
    ts_code: &str,
) -> Result<Option<String>, String> {
    let conn = open_database(app).map_err(|e| format!("db open: {e}"))?;
    migrate(&conn).map_err(|e| format!("migrate: {e}"))?;
    Ok(conn
        .query_row(
            "select occurred_at from account_events
             where ts_code = ?1 and event_type = 'watchlist_added'
             order by occurred_at desc limit 1",
            params![ts_code],
            |r| r.get::<_, String>(0),
        )
        .ok())
}

/// 取某 ts_code 的当前自选备注 —— 派生自最新的
/// `watchlist_added` / `watchlist_note_updated` 事件 payload。
/// 若期间有 `watchlist_removed`，返回 None（被删除后再 add 算新的开始）。
pub fn note_for(app: &AppHandle, ts_code: &str) -> Result<Option<String>, String> {
    let conn = open_database(app).map_err(|e| format!("db open: {e}"))?;
    migrate(&conn).map_err(|e| format!("migrate: {e}"))?;
    // 取该 ts_code 最近一次 add 之后的 add/note_updated/removed 事件链
    let mut stmt = conn
        .prepare(
            "select event_type, payload_json from account_events
             where ts_code = ?1
               and event_type in
                   ('watchlist_added','watchlist_note_updated','watchlist_removed')
             order by occurred_at desc",
        )
        .map_err(|e| format!("prepare: {e}"))?;
    let mut rows = stmt
        .query(params![ts_code])
        .map_err(|e| format!("query: {e}"))?;
    while let Some(r) = rows.next().map_err(|e| format!("next: {e}"))? {
        let et: String = r.get(0).map_err(|e| e.to_string())?;
        if et == "watchlist_removed" {
            return Ok(None);
        }
        let payload_str: String = r.get(1).map_err(|e| e.to_string())?;
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload_str) {
            if let Some(note) = v.get("note").and_then(|n| n.as_str()) {
                if !note.is_empty() {
                    return Ok(Some(note.to_string()));
                }
            }
        }
    }
    Ok(None)
}

/// 取初始化事件中的 initialCash 值（用于幂等校验）。
pub fn get_initial_cash(app: &AppHandle) -> Result<Option<f64>, String> {
    let conn = open_database(app).map_err(|e| format!("db open: {e}"))?;
    migrate(&conn).map_err(|e| format!("migrate: {e}"))?;
    let mut stmt = conn
        .prepare(
            "select payload_json from account_events
             where event_type = 'account_initialized'
             order by occurred_at asc limit 1",
        )
        .map_err(|e| format!("prepare: {e}"))?;
    let payload_str: Option<String> = stmt
        .query_row([], |r| r.get(0))
        .ok();
    let Some(s) = payload_str else { return Ok(None); };
    let v: serde_json::Value = serde_json::from_str(&s).map_err(|e| format!("parse: {e}"))?;
    Ok(v.get("initialCash").and_then(|x| x.as_f64()))
}

fn row_to_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<AccountEvent> {
    let event_type_str: String = r.get(1)?;
    let actor_str: String = r.get(7)?;
    let payload_str: String = r.get(8)?;
    let event_type = AccountEventType::parse(&event_type_str)
        .unwrap_or(AccountEventType::SnapshotRebuilt); // fallback：未知类型不应出现，因为有 CHECK 约束
    let actor = parse_actor(&actor_str);
    let payload: serde_json::Value =
        serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null);
    Ok(AccountEvent {
        event_id: r.get(0)?,
        event_type,
        order_id: r.get(2)?,
        fill_id: r.get(3)?,
        position_id: r.get(4)?,
        ts_code: r.get(5)?,
        reason: r.get(6)?,
        actor,
        payload,
        occurred_at: r.get(9)?,
    })
}

fn parse_actor(s: &str) -> AccountActor {
    match s {
        "agent" => AccountActor::Agent,
        "system" => AccountActor::System,
        "user" => AccountActor::User,
        _ => AccountActor::System,
    }
}
