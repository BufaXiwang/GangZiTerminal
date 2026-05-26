//! `account_triggers` 持久化。

use crate::domain::account::trigger::{AccountTrigger, AccountTriggerType};
use crate::infrastructure::db::helpers::now;
use crate::infrastructure::db::{migrate, open_database};
use rusqlite::{params, Connection};
use tauri::AppHandle;

fn conn(app: &AppHandle) -> Result<Connection, String> {
    let c = open_database(app)?;
    migrate(&c)?;
    Ok(c)
}

fn parse_type(s: &str) -> Option<AccountTriggerType> {
    Some(match s {
        "stop_loss" => AccountTriggerType::StopLoss,
        "take_profit" => AccountTriggerType::TakeProfit,
        "time_stop" => AccountTriggerType::TimeStop,
        "order_filled" => AccountTriggerType::OrderFilled,
        "order_rejected" => AccountTriggerType::OrderRejected,
        "order_expired" => AccountTriggerType::OrderExpired,
        "invalidated" => AccountTriggerType::Invalidated,
        _ => return None,
    })
}

fn opt_json<T: serde::Serialize>(v: &Option<T>) -> Result<Option<String>, String> {
    v.as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| format!("trigger json 序列化失败：{e}"))
}

fn warnings_json(
    ws: &[crate::domain::shared::WarningCode],
) -> Result<Option<String>, String> {
    if ws.is_empty() {
        return Ok(None);
    }
    serde_json::to_string(ws)
        .map(Some)
        .map_err(|e| format!("trigger warnings 序列化失败：{e}"))
}

/// 写入 / 幂等保留已存在的 trigger。返回 true 表示新写入。
pub fn upsert_pending(app: &AppHandle, trig: &AccountTrigger) -> Result<bool, String> {
    let c = conn(app)?;
    let existed: bool = c
        .query_row(
            "select 1 from account_triggers where trigger_id = ?1",
            params![trig.trigger_id],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if existed {
        return Ok(false);
    }
    let threshold_json = opt_json(&trig.threshold)?;
    let quote_freshness_json = opt_json(&trig.quote_freshness)?;
    let warnings_json = warnings_json(&trig.warnings)?;
    c.execute(
        "insert into account_triggers(
            trigger_id, trigger_type, order_id, position_id, ts_code, price,
            threshold_json, quote_freshness_json, warnings_json,
            event_id, handled, occurred_at, created_at, updated_at
         ) values (?1,?2,?3,?4,?5,?6, ?7,?8,?9, ?10, 0, ?11, ?12, ?12)",
        params![
            trig.trigger_id,
            trig.trigger_type.as_str(),
            trig.order_id,
            trig.position_id,
            trig.ts_code,
            trig.price,
            threshold_json,
            quote_freshness_json,
            warnings_json,
            trig.event_id,
            trig.occurred_at,
            now(),
        ],
    )
    .map_err(|e| format!("upsert account_trigger 失败：{e}"))?;
    Ok(true)
}

pub fn mark_handled(app: &AppHandle, trigger_id: &str) -> Result<bool, String> {
    let c = conn(app)?;
    let n = c
        .execute(
            "update account_triggers
             set handled = 1, updated_at = ?2
             where trigger_id = ?1 and handled = 0",
            params![trigger_id, now()],
        )
        .map_err(|e| format!("mark_handled 失败：{e}"))?;
    Ok(n > 0)
}

fn parse_row(r: &rusqlite::Row<'_>) -> Result<Option<AccountTrigger>, String> {
    let type_str: String = r.get(1).map_err(|e| e.to_string())?;
    let Some(trigger_type) = parse_type(&type_str) else {
        return Ok(None);
    };
    let threshold_json: Option<String> = r.get(6).map_err(|e| e.to_string())?;
    let quote_freshness_json: Option<String> = r.get(7).map_err(|e| e.to_string())?;
    let warnings_json: Option<String> = r.get(8).map_err(|e| e.to_string())?;
    let threshold = threshold_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());
    let quote_freshness = quote_freshness_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());
    let warnings: Vec<crate::domain::shared::WarningCode> = warnings_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    Ok(Some(AccountTrigger {
        trigger_id: r.get(0).map_err(|e| e.to_string())?,
        trigger_type,
        order_id: r.get(2).map_err(|e| e.to_string())?,
        position_id: r.get(3).map_err(|e| e.to_string())?,
        ts_code: r.get(4).map_err(|e| e.to_string())?,
        price: r.get(5).map_err(|e| e.to_string())?,
        threshold,
        quote_freshness,
        warnings,
        event_id: r.get(9).map_err(|e| e.to_string())?,
        handled: r.get::<_, i64>(10).map_err(|e| e.to_string())? != 0,
        occurred_at: r.get(11).map_err(|e| e.to_string())?,
    }))
}

const SELECT_COLUMNS: &str = "trigger_id, trigger_type, order_id, position_id, ts_code, price,
        threshold_json, quote_freshness_json, warnings_json,
        event_id, handled, occurred_at";

pub fn list_pending(app: &AppHandle, limit: i64) -> Result<Vec<AccountTrigger>, String> {
    let c = conn(app)?;
    let sql = format!(
        "select {SELECT_COLUMNS}
         from account_triggers
         where handled = 0
         order by occurred_at asc limit ?1"
    );
    let mut stmt = c.prepare(&sql).map_err(|e| format!("prepare 失败：{e}"))?;
    let mut rows = stmt
        .query(params![limit])
        .map_err(|e| format!("query 失败：{e}"))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(|e| format!("next 失败：{e}"))? {
        if let Some(trig) = parse_row(r)? {
            out.push(trig);
        }
    }
    Ok(out)
}

/// 按 handled 过滤列出 triggers（spec §4 `fetch_account.include.triggers` /
/// `triggerHandled` 实参）。`handled = None` 表示不过滤。
pub fn list_filtered(
    app: &AppHandle,
    handled: Option<bool>,
    limit: i64,
    offset: i64,
) -> Result<Vec<AccountTrigger>, String> {
    let c = conn(app)?;
    let (where_clause, want_filter) = match handled {
        Some(_) => ("where handled = ?1", true),
        None => ("", false),
    };
    let sql = format!(
        "select {SELECT_COLUMNS}
         from account_triggers {where_clause}
         order by occurred_at desc
         limit ?{lim} offset ?{off}",
        lim = if want_filter { 2 } else { 1 },
        off = if want_filter { 3 } else { 2 },
    );
    let mut stmt = c.prepare(&sql).map_err(|e| format!("prepare 失败：{e}"))?;
    let mut rows = if let Some(h) = handled {
        stmt.query(params![if h { 1 } else { 0 }, limit, offset])
    } else {
        stmt.query(params![limit, offset])
    }
    .map_err(|e| format!("query 失败：{e}"))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(|e| format!("next 失败：{e}"))? {
        if let Some(trig) = parse_row(r)? {
            out.push(trig);
        }
    }
    Ok(out)
}

pub fn count_pending(app: &AppHandle) -> Result<i64, String> {
    let c = conn(app)?;
    let n: i64 = c
        .query_row(
            "select count(*) from account_triggers where handled = 0",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("count 失败：{e}"))?;
    Ok(n)
}
