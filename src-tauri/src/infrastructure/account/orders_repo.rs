//! `account_orders` 持久化 —— spec `account-module.md §2 Order`。
//!
//! 当前用例：
//! - `place_order(market)` 即时成交：写一条 status=filled，并由调用方串接 fill / position
//! - `place_order(limit)` 挂单：写 status=pending，等后续评估
//! - `cancel_order`：pending / partially_filled → cancelled
//! - 启动时把 status=pending 的过期订单做 expiration sweep（未来 scheduler 接）

use rusqlite::{params, Connection};
use tauri::AppHandle;

use crate::domain::account::order::{Order, OrderIntent, OrderSide, OrderStatus, OrderType};
use crate::domain::account::events::AccountActor;
use crate::domain::shared::{OccurredAt, Shares, Yuan};
use crate::infrastructure::db::helpers::now;
use crate::infrastructure::db::{migrate, open_database};

fn conn(app: &AppHandle) -> Result<Connection, String> {
    let c = open_database(app)?;
    migrate(&c)?;
    Ok(c)
}

fn actor_str(a: AccountActor) -> &'static str {
    a.as_str()
}

fn intent_str(i: OrderIntent) -> &'static str {
    match i {
        OrderIntent::OpenPosition => "open_position",
        OrderIntent::ScaleIn => "scale_in",
        OrderIntent::ScaleOut => "scale_out",
        OrderIntent::ClosePosition => "close_position",
        OrderIntent::DirectOrder => "direct_order",
    }
}

fn side_str(s: OrderSide) -> &'static str {
    match s {
        OrderSide::Buy => "buy",
        OrderSide::Sell => "sell",
    }
}

fn order_type_str(t: OrderType) -> &'static str {
    match t {
        OrderType::Market => "market",
        OrderType::Limit => "limit",
    }
}

fn parse_status(s: &str) -> Option<OrderStatus> {
    Some(match s {
        "pending" => OrderStatus::Pending,
        "partially_filled" => OrderStatus::PartiallyFilled,
        "filled" => OrderStatus::Filled,
        "cancelled" => OrderStatus::Cancelled,
        "rejected" => OrderStatus::Rejected,
        "expired" => OrderStatus::Expired,
        _ => return None,
    })
}

fn occurred_to_rfc3339(t: &OccurredAt) -> String {
    chrono::DateTime::from_timestamp_millis(t.value())
        .map(|d| d.to_rfc3339())
        .unwrap_or_else(|| String::new())
}

fn parse_rfc3339(s: &str) -> Option<OccurredAt> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| OccurredAt::new(d.timestamp_millis()))
}

pub fn insert(app: &AppHandle, order: &Order) -> Result<(), String> {
    // spec account-module.md §2「`user` 只允许用于自选维护事件，不允许创建订单」
    if matches!(order.actor, AccountActor::User) {
        return Err("invalid_input: user actor 不允许创建 Order".into());
    }
    let c = conn(app)?;
    c.execute(
        "insert into account_orders(
            order_id, ts_code, side, order_type, limit_price, quantity, filled_quantity,
            status, intent, position_id, reason, actor, created_at, updated_at, expires_at
         ) values (?1,?2,?3,?4,?5,?6,?7, ?8,?9,?10,?11,?12, ?13,?14,?15)",
        params![
            order.order_id,
            order.ts_code.as_str(),
            side_str(order.side),
            order_type_str(order.order_type),
            order.limit_price.as_ref().map(|p| p.value()),
            order.quantity.value(),
            order.filled_quantity.value(),
            order.status.as_str(),
            intent_str(order.intent),
            order.position_id,
            order.reason,
            actor_str(order.actor),
            occurred_to_rfc3339(&order.created_at),
            occurred_to_rfc3339(&order.updated_at),
            order.expires_at.as_ref().map(occurred_to_rfc3339),
        ],
    )
    .map_err(|e| format!("写 account_order 失败：{e}"))?;
    Ok(())
}

pub fn update_status(
    app: &AppHandle,
    order_id: &str,
    status: OrderStatus,
    filled_qty: Option<i64>,
) -> Result<bool, String> {
    let c = conn(app)?;
    let ts = now(); // RFC3339
    let n = c
        .execute(
            "update account_orders
             set status = ?2,
                 filled_quantity = coalesce(?3, filled_quantity),
                 updated_at = ?4
             where order_id = ?1",
            params![order_id, status.as_str(), filled_qty, ts],
        )
        .map_err(|e| format!("update account_order 失败：{e}"))?;
    Ok(n > 0)
}

pub fn attach_position(
    app: &AppHandle,
    order_id: &str,
    position_id: &str,
) -> Result<(), String> {
    let c = conn(app)?;
    let ts = now(); // RFC3339
    c.execute(
        "update account_orders set position_id = ?2, updated_at = ?3 where order_id = ?1",
        params![order_id, position_id, ts],
    )
    .map_err(|e| format!("attach position 失败：{e}"))?;
    Ok(())
}

pub fn get(app: &AppHandle, order_id: &str) -> Result<Option<Order>, String> {
    let c = conn(app)?;
    let mut stmt = c
        .prepare(
            "select order_id, ts_code, side, order_type, limit_price, quantity,
                    filled_quantity, status, intent, position_id, reason, actor,
                    created_at, updated_at, expires_at
             from account_orders where order_id = ?1",
        )
        .map_err(|e| format!("prepare 失败：{e}"))?;
    let mut rows = stmt
        .query(params![order_id])
        .map_err(|e| format!("query 失败：{e}"))?;
    if let Some(r) = rows.next().map_err(|e| format!("next 失败：{e}"))? {
        Ok(parse_row(r))
    } else {
        Ok(None)
    }
}

pub fn list_active(app: &AppHandle, limit: i64, offset: i64) -> Result<Vec<Order>, String> {
    list_filtered(app, Some(&["pending", "partially_filled"]), limit, offset)
}

pub fn list_all(app: &AppHandle, limit: i64, offset: i64) -> Result<Vec<Order>, String> {
    list_filtered(app, None, limit, offset)
}

pub fn list_by_status(
    app: &AppHandle,
    statuses: &[&str],
    limit: i64,
    offset: i64,
) -> Result<Vec<Order>, String> {
    if statuses.is_empty() {
        return Ok(Vec::new());
    }
    list_filtered(app, Some(statuses), limit, offset)
}

fn list_filtered(
    app: &AppHandle,
    statuses: Option<&[&str]>,
    limit: i64,
    offset: i64,
) -> Result<Vec<Order>, String> {
    let c = conn(app)?;
    let cols = "order_id, ts_code, side, order_type, limit_price, quantity, filled_quantity,
                status, intent, position_id, reason, actor, created_at, updated_at, expires_at";
    let (sql, want_filter) = match statuses {
        Some(_) => (
            format!(
                "select {cols}
                 from account_orders
                 where status in (PLACEHOLDER)
                 order by updated_at desc limit ?LIM offset ?OFF"
            ),
            true,
        ),
        None => (
            format!(
                "select {cols} from account_orders order by updated_at desc limit ?LIM offset ?OFF"
            ),
            false,
        ),
    };
    let final_sql = if want_filter {
        let placeholders = statuses
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        sql.replace("PLACEHOLDER", &placeholders)
            .replace("?LIM", &format!("?{}", statuses.unwrap().len() + 1))
            .replace("?OFF", &format!("?{}", statuses.unwrap().len() + 2))
    } else {
        sql.replace("?LIM", "?1").replace("?OFF", "?2")
    };
    let mut stmt = c.prepare(&final_sql).map_err(|e| format!("prepare 失败：{e}"))?;
    let mut out = Vec::new();
    let push_rows = |mut rows: rusqlite::Rows<'_>,
                     out: &mut Vec<Order>|
     -> Result<(), String> {
        while let Some(r) = rows.next().map_err(|e| format!("next 失败：{e}"))? {
            if let Some(o) = parse_row(r) {
                out.push(o);
            }
        }
        Ok(())
    };
    match statuses {
        Some(ss) => {
            let mut params_dyn: Vec<rusqlite::types::Value> = Vec::new();
            for s in ss {
                params_dyn.push(rusqlite::types::Value::Text((*s).to_string()));
            }
            params_dyn.push(rusqlite::types::Value::Integer(limit));
            params_dyn.push(rusqlite::types::Value::Integer(offset));
            let rows = stmt
                .query(rusqlite::params_from_iter(params_dyn.iter()))
                .map_err(|e| format!("query 失败：{e}"))?;
            push_rows(rows, &mut out)?;
        }
        None => {
            let rows = stmt
                .query(params![limit, offset])
                .map_err(|e| format!("query 失败：{e}"))?;
            push_rows(rows, &mut out)?;
        }
    }
    Ok(out)
}

fn parse_row(r: &rusqlite::Row<'_>) -> Option<Order> {
    use crate::domain::shared::TsCode;
    let order_id: String = r.get(0).ok()?;
    let ts_code: String = r.get(1).ok()?;
    let side: String = r.get(2).ok()?;
    let order_type: String = r.get(3).ok()?;
    let limit_price: Option<f64> = r.get(4).ok()?;
    let quantity: i64 = r.get(5).ok()?;
    let filled_quantity: i64 = r.get(6).ok()?;
    let status: String = r.get(7).ok()?;
    let intent: String = r.get(8).ok()?;
    let position_id: Option<String> = r.get(9).ok()?;
    let reason: Option<String> = r.get(10).ok()?;
    let actor: String = r.get(11).ok()?;
    let created_at: String = r.get(12).ok()?;
    let updated_at: String = r.get(13).ok()?;
    let expires_at: Option<String> = r.get(14).ok()?;
    Some(Order {
        order_id,
        ts_code: TsCode::from_unchecked(ts_code),
        side: match side.as_str() {
            "buy" => OrderSide::Buy,
            "sell" => OrderSide::Sell,
            _ => return None,
        },
        order_type: match order_type.as_str() {
            "market" => OrderType::Market,
            "limit" => OrderType::Limit,
            _ => return None,
        },
        limit_price: limit_price.map(Yuan::from_unchecked),
        quantity: Shares::from_unchecked(quantity),
        filled_quantity: Shares::from_unchecked(filled_quantity),
        status: parse_status(&status)?,
        intent: match intent.as_str() {
            "open_position" => OrderIntent::OpenPosition,
            "scale_in" => OrderIntent::ScaleIn,
            "scale_out" => OrderIntent::ScaleOut,
            "close_position" => OrderIntent::ClosePosition,
            "direct_order" => OrderIntent::DirectOrder,
            _ => return None,
        },
        position_id,
        reason,
        actor: match actor.as_str() {
            "agent" => AccountActor::Agent,
            "system" => AccountActor::System,
            "user" => AccountActor::User,
            _ => return None,
        },
        created_at: parse_rfc3339(&created_at).unwrap_or_else(|| OccurredAt::new(0)),
        updated_at: parse_rfc3339(&updated_at).unwrap_or_else(|| OccurredAt::new(0)),
        expires_at: expires_at.and_then(|s| parse_rfc3339(&s)),
    })
}
