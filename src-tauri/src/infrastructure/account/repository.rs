//! AccountRepository — Account 真源表的读写。
//!
//! Spec: docs/design/account-module.md §2 / §3
//!
//! 设计：所有写操作通过 `tx_with` 封装为一个事务；调用方负责把多个 op 组合到一个 tx 里。
//! 读取统一使用 `AppDb::with`。

use crate::domain::account::events::{AccountEvent, AccountEventType};
use crate::domain::account::triggers::{AccountTrigger, AccountTriggerType};
use crate::domain::account::types::{
    Order, OrderIntent, OrderSide, OrderStatus, OrderType, Position, PositionLot,
    PositionProtection, PositionStatus, TradeFill, TradingActor, WatchlistItem,
};
use crate::domain::shared::{
    Freshness, FreshnessStatus, Money, OccurredAt, Price, Shares, TradeDate, TsCode, WarningCode,
};
use crate::infrastructure::db::AppDb;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use rust_decimal::Decimal;
use serde_json::Value as JsonValue;
use std::str::FromStr;

pub struct AccountRepository<'a> {
    db: &'a AppDb,
}

/// 同账户事务内最多一次写入的事件列表；调用方负责按 spec 顺序 append。
pub struct AccountTxOps<'a, 'b> {
    pub tx: &'a Transaction<'b>,
}

#[derive(Debug, Clone)]
pub struct AccountMeta {
    pub initial_cash: Money,
    pub cash: Money,
    pub initialized_at: OccurredAt,
    pub updated_at: OccurredAt,
}

#[derive(Debug, Clone)]
pub struct FreezeEntry {
    pub order_id: String,
    pub ts_code: TsCode,
    pub side: OrderSide,
    pub frozen_cash: Money,
    pub frozen_shares: Shares,
    pub frozen_lots: Vec<FrozenLot>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FrozenLot {
    pub lot_id: String,
    pub quantity: i64,
}

impl<'a> AccountRepository<'a> {
    pub fn new(db: &'a AppDb) -> Self {
        Self { db }
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn parse_decimal(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap_or(Decimal::ZERO)
    }

    pub fn db(&self) -> &AppDb {
        self.db
    }

    // ------------------------------------------------------------------
    // Account meta
    // ------------------------------------------------------------------

    pub fn get_meta(&self) -> rusqlite::Result<Option<AccountMeta>> {
        self.db.with(|c| Self::read_meta(c))
    }

    pub(crate) fn read_meta(conn: &Connection) -> rusqlite::Result<Option<AccountMeta>> {
        conn.query_row(
            "SELECT initial_cash, cash, initialized_at, updated_at FROM account_meta WHERE id = 1",
            [],
            |row| {
                Ok(AccountMeta {
                    initial_cash: Money(Self::parse_decimal(&row.get::<_, String>(0)?)),
                    cash: Money(Self::parse_decimal(&row.get::<_, String>(1)?)),
                    initialized_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(2)?)
                        .map(|d| d.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                    updated_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(3)?)
                        .map(|d| d.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                })
            },
        )
        .optional()
    }

    pub fn insert_meta(&self, initial_cash: Money, now: OccurredAt) -> rusqlite::Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT OR IGNORE INTO account_meta (id, initial_cash, cash, initialized_at, updated_at)
                 VALUES (1, ?, ?, ?, ?)",
                params![
                    initial_cash.0.to_string(),
                    initial_cash.0.to_string(),
                    now.to_rfc3339(),
                    now.to_rfc3339()
                ],
            )?;
            Ok(())
        })
    }

    /// Insert meta row inside an existing transaction (atomic with event append).
    ///
    /// Spec: account-module.md §3 数据流 — 所有状态变化必须先写 `account_events`,
    /// 再更新派生缓存。`account_initialized` event 与 `account_meta` 写入必须原子。
    pub fn insert_meta_in_tx(
        tx: &Transaction<'_>,
        initial_cash: Money,
        now: OccurredAt,
    ) -> rusqlite::Result<()> {
        tx.execute(
            "INSERT OR IGNORE INTO account_meta (id, initial_cash, cash, initialized_at, updated_at)
             VALUES (1, ?, ?, ?, ?)",
            params![
                initial_cash.0.to_string(),
                initial_cash.0.to_string(),
                now.to_rfc3339(),
                now.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn update_cash(&self, cash: Money, now: OccurredAt) -> rusqlite::Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE account_meta SET cash = ?, updated_at = ? WHERE id = 1",
                params![cash.0.to_string(), now.to_rfc3339()],
            )?;
            Ok(())
        })
    }

    /// 同事务内更新 cash。
    ///
    /// Spec: account-module.md §3 数据流: "所有状态变化必须先写 account_events,
    /// 再更新 / 派生订单, 仓位, snapshot"。`meta.cash` 是派生缓存，与 fill 事件
    /// 必须原子更新；否则 tx 提交后 update_cash 失败会让缓存与事件源不一致。
    pub fn update_cash_in_tx(tx: &Transaction<'_>, cash: Money, now: OccurredAt) -> rusqlite::Result<()> {
        tx.execute(
            "UPDATE account_meta SET cash = ?, updated_at = ? WHERE id = 1",
            params![cash.0.to_string(), now.to_rfc3339()],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Event append + sequence
    // ------------------------------------------------------------------

    /// Allocate next event seq inside transaction.
    pub fn next_event_seq(tx: &Transaction<'_>) -> rusqlite::Result<i64> {
        let next: i64 =
            tx.query_row("SELECT next FROM account_event_seq WHERE id = 1", [], |r| r.get(0))?;
        tx.execute(
            "UPDATE account_event_seq SET next = next + 1 WHERE id = 1",
            [],
        )?;
        Ok(next)
    }

    /// Append a single AccountEvent inside an existing tx; returns event_id.
    pub fn append_event(tx: &Transaction<'_>, ev: &AccountEvent) -> rusqlite::Result<()> {
        let seq = Self::next_event_seq(tx)?;
        tx.execute(
            "INSERT INTO account_events
             (event_id, event_type, order_id, fill_id, position_id, ts_code,
              reason, actor, payload_json, occurred_at, seq)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                ev.event_id,
                ev.event_type.as_str(),
                ev.order_id,
                ev.fill_id,
                ev.position_id,
                ev.ts_code.as_ref().map(|c| c.as_str().to_string()),
                ev.reason,
                ev.actor,
                serde_json::to_string(&ev.payload).unwrap_or_else(|_| "null".into()),
                ev.occurred_at.to_rfc3339(),
                seq,
            ],
        )?;
        Ok(())
    }

    pub fn list_events(
        &self,
        limit: u32,
        offset: u32,
    ) -> rusqlite::Result<Vec<AccountEvent>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT event_id, event_type, order_id, fill_id, position_id, ts_code,
                        reason, actor, payload_json, occurred_at
                 FROM account_events
                 ORDER BY seq DESC
                 LIMIT ? OFFSET ?",
            )?;
            let rows = stmt
                .query_map(params![limit, offset], Self::map_event_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    pub fn count_events(&self) -> rusqlite::Result<u32> {
        self.db.with(|c| {
            c.query_row("SELECT COUNT(*) FROM account_events", [], |r| {
                r.get::<_, i64>(0).map(|n| n as u32)
            })
        })
    }

    /// 检查 account_initialized 是否已存在。
    pub fn has_account_initialized(&self) -> rusqlite::Result<bool> {
        self.db.with(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM account_events WHERE event_type = 'account_initialized'",
                [],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        })
    }

    fn map_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AccountEvent> {
        let ts_code_str: Option<String> = row.get(5)?;
        let payload_str: String = row.get(8)?;
        let payload: JsonValue = serde_json::from_str(&payload_str).unwrap_or(JsonValue::Null);
        Ok(AccountEvent {
            event_id: row.get(0)?,
            event_type: AccountEventType::from_str(&row.get::<_, String>(1)?)
                .unwrap_or(AccountEventType::SnapshotRebuilt),
            order_id: row.get(2)?,
            fill_id: row.get(3)?,
            position_id: row.get(4)?,
            ts_code: ts_code_str.and_then(|s| TsCode::parse(&s).ok()),
            reason: row.get(6)?,
            actor: row.get(7)?,
            payload,
            occurred_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(9)?)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
        })
    }

    // ------------------------------------------------------------------
    // Orders
    // ------------------------------------------------------------------

    pub fn upsert_order(tx: &Transaction<'_>, order: &Order) -> rusqlite::Result<()> {
        tx.execute(
            "INSERT INTO account_orders
             (order_id, ts_code, side, order_type, limit_price, quantity, filled_quantity,
              status, intent, position_id, reason, actor, created_at, updated_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(order_id) DO UPDATE SET
               filled_quantity = excluded.filled_quantity,
               status          = excluded.status,
               position_id     = COALESCE(excluded.position_id, account_orders.position_id),
               updated_at      = excluded.updated_at,
               expires_at      = excluded.expires_at",
            params![
                order.order_id,
                order.ts_code.as_str(),
                order_side_str(order.side),
                order_type_str(order.order_type),
                order.limit_price.map(|p| p.0.to_string()),
                order.quantity.0,
                order.filled_quantity.0,
                order_status_str(order.status),
                order_intent_str(order.intent),
                order.position_id,
                order.reason,
                "agent",
                order.created_at.to_rfc3339(),
                order.updated_at.to_rfc3339(),
                order.expires_at.map(|t| t.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    pub fn get_order(&self, order_id: &str) -> rusqlite::Result<Option<Order>> {
        self.db.with(|c| {
            c.query_row(
                "SELECT order_id, ts_code, side, order_type, limit_price, quantity,
                        filled_quantity, status, intent, position_id, reason,
                        created_at, updated_at, expires_at
                 FROM account_orders WHERE order_id = ?",
                params![order_id],
                Self::map_order_row,
            )
            .optional()
        })
    }

    pub fn list_orders(
        &self,
        statuses: Option<&[OrderStatus]>,
        limit: u32,
        offset: u32,
    ) -> rusqlite::Result<Vec<Order>> {
        self.db.with(|c| {
            let (sql, params_vec): (String, Vec<String>) = if let Some(sts) = statuses {
                if sts.is_empty() {
                    return Ok(Vec::new());
                }
                let placeholders = sts.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let sql = format!(
                    "SELECT order_id, ts_code, side, order_type, limit_price, quantity,
                            filled_quantity, status, intent, position_id, reason,
                            created_at, updated_at, expires_at
                     FROM account_orders WHERE status IN ({})
                     ORDER BY updated_at DESC, order_id LIMIT ? OFFSET ?",
                    placeholders
                );
                let mut pv: Vec<String> =
                    sts.iter().map(|s| order_status_str(*s).into()).collect();
                pv.push(limit.to_string());
                pv.push(offset.to_string());
                (sql, pv)
            } else {
                (
                    "SELECT order_id, ts_code, side, order_type, limit_price, quantity,
                            filled_quantity, status, intent, position_id, reason,
                            created_at, updated_at, expires_at
                     FROM account_orders ORDER BY updated_at DESC, order_id LIMIT ? OFFSET ?"
                        .into(),
                    vec![limit.to_string(), offset.to_string()],
                )
            };
            let mut stmt = c.prepare(&sql)?;
            let params_dyn: Vec<&dyn rusqlite::ToSql> =
                params_vec.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
            let rows = stmt
                .query_map(params_dyn.as_slice(), Self::map_order_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// 列出仍需调度评估的活跃订单。
    ///
    /// Spec: account-module.md §2 订单模型 line 168-170:
    ///   - market `partially_filled` 是终态（剩余量已同事务自动取消）。
    ///   - limit `partially_filled` 是中间态（剩余仍 pending）。
    ///
    /// 因此 evaluate_account_triggers 只需要扫描：
    ///   - status = 'pending'（所有 limit pending，期望成交 / 过期）
    ///   - status = 'partially_filled' AND order_type = 'limit'（剩余量继续撮合）
    pub fn list_active_orders(&self) -> rusqlite::Result<Vec<Order>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT order_id, ts_code, side, order_type, limit_price, quantity,
                        filled_quantity, status, intent, position_id, reason,
                        created_at, updated_at, expires_at
                 FROM account_orders
                 WHERE status = 'pending'
                    OR (status = 'partially_filled' AND order_type = 'limit')
                 ORDER BY updated_at ASC, order_id ASC",
            )?;
            let rows = stmt
                .query_map([], Self::map_order_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// 查找指定 ts_code 的未终态 `open_position` 订单（pending / partially_filled）。
    ///
    /// Spec: account-module.md §2 仓位模型规则:
    ///   同一 ts_code 同时只允许一个未终态的 open_position 订单。
    pub fn find_pending_open_position_order(
        &self,
        ts_code: &TsCode,
    ) -> rusqlite::Result<Option<Order>> {
        // Spec §2 line 320: 只 limit pending / partially_filled 算未终态；
        // market 订单的 partially_filled 已是终态（剩余量同事务自动取消），
        // 不应该用来阻止第二次 open_position（也不会，因为 position 已生成会先撞墙）。
        self.db.with(|c| {
            c.query_row(
                "SELECT order_id, ts_code, side, order_type, limit_price, quantity,
                        filled_quantity, status, intent, position_id, reason,
                        created_at, updated_at, expires_at
                 FROM account_orders
                 WHERE ts_code = ? AND intent = 'open_position'
                   AND order_type = 'limit'
                   AND status IN ('pending','partially_filled')
                 ORDER BY created_at ASC, order_id ASC
                 LIMIT 1",
                params![ts_code.as_str()],
                Self::map_order_row,
            )
            .optional()
        })
    }

    /// 计算未完成订单数。
    ///
    /// Spec: account-module.md §2 账户快照 pending_order_count = 未完成订单数。
    /// market `partially_filled` 是终态（spec §2 订单模型 line 169-170），不能算"未完成"。
    pub fn count_pending_orders(&self) -> rusqlite::Result<u32> {
        self.db.with(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM account_orders
                 WHERE status = 'pending'
                    OR (status = 'partially_filled' AND order_type = 'limit')",
                [],
                |r| r.get::<_, i64>(0).map(|n| n as u32),
            )
        })
    }

    fn map_order_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Order> {
        let ts_code = TsCode::parse(&row.get::<_, String>(1)?)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e)))?;
        Ok(Order {
            order_id: row.get(0)?,
            ts_code,
            side: order_side_from_str(&row.get::<_, String>(2)?),
            order_type: order_type_from_str(&row.get::<_, String>(3)?),
            limit_price: row.get::<_, Option<String>>(4)?
                .map(|s| Price(Self::parse_decimal(&s))),
            quantity: Shares(row.get::<_, i64>(5)?),
            filled_quantity: Shares(row.get::<_, i64>(6)?),
            status: order_status_from_str(&row.get::<_, String>(7)?),
            intent: order_intent_from_str(&row.get::<_, String>(8)?),
            position_id: row.get(9)?,
            reason: row.get(10)?,
            actor: TradingActor::Agent,
            created_at: parse_rfc3339(&row.get::<_, String>(11)?),
            updated_at: parse_rfc3339(&row.get::<_, String>(12)?),
            expires_at: row.get::<_, Option<String>>(13)?.map(|s| parse_rfc3339(&s)),
        })
    }

    // ------------------------------------------------------------------
    // Fills
    // ------------------------------------------------------------------

    pub fn insert_fill(tx: &Transaction<'_>, fill: &TradeFill) -> rusqlite::Result<()> {
        let seq = Self::next_event_seq(tx)?;
        tx.execute(
            "INSERT INTO account_fills
             (fill_id, order_id, position_id, ts_code, side, price, quantity,
              commission, stamp_tax, transfer_fee, occurred_at, seq)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                fill.fill_id,
                fill.order_id,
                fill.position_id,
                fill.ts_code.as_str(),
                order_side_str(fill.side),
                fill.price.0.to_string(),
                fill.quantity.0,
                fill.commission.0.to_string(),
                fill.stamp_tax.0.to_string(),
                fill.transfer_fee.0.to_string(),
                fill.occurred_at.to_rfc3339(),
                seq,
            ],
        )?;
        Ok(())
    }

    pub fn list_fills_by_order(&self, order_id: &str) -> rusqlite::Result<Vec<TradeFill>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT fill_id, order_id, position_id, ts_code, side, price, quantity,
                        commission, stamp_tax, transfer_fee, occurred_at
                 FROM account_fills WHERE order_id = ? ORDER BY seq",
            )?;
            let rows = stmt
                .query_map(params![order_id], Self::map_fill_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    fn map_fill_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TradeFill> {
        let ts_code = TsCode::parse(&row.get::<_, String>(3)?)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e)))?;
        Ok(TradeFill {
            fill_id: row.get(0)?,
            order_id: row.get(1)?,
            position_id: row.get(2)?,
            ts_code,
            side: order_side_from_str(&row.get::<_, String>(4)?),
            price: Price(Self::parse_decimal(&row.get::<_, String>(5)?)),
            quantity: Shares(row.get::<_, i64>(6)?),
            commission: Money(Self::parse_decimal(&row.get::<_, String>(7)?)),
            stamp_tax: Money(Self::parse_decimal(&row.get::<_, String>(8)?)),
            transfer_fee: Money(Self::parse_decimal(&row.get::<_, String>(9)?)),
            occurred_at: parse_rfc3339(&row.get::<_, String>(10)?),
        })
    }

    // ------------------------------------------------------------------
    // Positions
    // ------------------------------------------------------------------

    pub fn upsert_position(tx: &Transaction<'_>, pos: &Position) -> rusqlite::Result<()> {
        tx.execute(
            "INSERT INTO account_positions
             (position_id, ts_code, name, status, quantity, avg_cost, realized_pnl,
              opened_at, closed_at, actor, reasoning)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(position_id) DO UPDATE SET
               quantity = excluded.quantity,
               avg_cost = excluded.avg_cost,
               status   = excluded.status,
               realized_pnl = excluded.realized_pnl,
               closed_at = excluded.closed_at,
               reasoning = COALESCE(excluded.reasoning, account_positions.reasoning)",
            params![
                pos.position_id,
                pos.ts_code.as_str(),
                pos.name,
                position_status_str(pos.status),
                pos.quantity.0,
                pos.avg_cost.0.to_string(),
                pos.realized_pnl.0.to_string(),
                pos.opened_at.to_rfc3339(),
                pos.closed_at.map(|t| t.to_rfc3339()),
                "agent",
                pos.reasoning,
            ],
        )?;
        Ok(())
    }

    pub fn find_open_position_by_ts_code(
        &self,
        ts_code: &TsCode,
    ) -> rusqlite::Result<Option<Position>> {
        self.db.with(|c| {
            c.query_row(
                "SELECT position_id, ts_code, name, status, quantity, avg_cost, realized_pnl,
                        opened_at, closed_at, reasoning
                 FROM account_positions
                 WHERE ts_code = ? AND status = 'open' LIMIT 1",
                params![ts_code.as_str()],
                Self::map_position_row_basic,
            )
            .optional()
        })
    }

    pub fn get_position(&self, position_id: &str) -> rusqlite::Result<Option<Position>> {
        self.db.with(|c| {
            c.query_row(
                "SELECT position_id, ts_code, name, status, quantity, avg_cost, realized_pnl,
                        opened_at, closed_at, reasoning
                 FROM account_positions WHERE position_id = ?",
                params![position_id],
                Self::map_position_row_basic,
            )
            .optional()
        })
    }

    pub fn list_positions(
        &self,
        status: Option<PositionStatus>,
        limit: u32,
        offset: u32,
    ) -> rusqlite::Result<Vec<Position>> {
        self.db.with(|c| {
            if let Some(s) = status {
                let mut stmt = c.prepare(
                    "SELECT position_id, ts_code, name, status, quantity, avg_cost, realized_pnl,
                            opened_at, closed_at, reasoning
                     FROM account_positions WHERE status = ?
                     ORDER BY opened_at DESC LIMIT ? OFFSET ?",
                )?;
                let rows = stmt
                    .query_map(
                        params![position_status_str(s), limit, offset],
                        Self::map_position_row_basic,
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            } else {
                let mut stmt = c.prepare(
                    "SELECT position_id, ts_code, name, status, quantity, avg_cost, realized_pnl,
                            opened_at, closed_at, reasoning
                     FROM account_positions
                     ORDER BY opened_at DESC LIMIT ? OFFSET ?",
                )?;
                let rows = stmt
                    .query_map(params![limit, offset], Self::map_position_row_basic)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            }
        })
    }

    fn map_position_row_basic(row: &rusqlite::Row<'_>) -> rusqlite::Result<Position> {
        let ts_code = TsCode::parse(&row.get::<_, String>(1)?)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e)))?;
        Ok(Position {
            position_id: row.get(0)?,
            ts_code,
            name: row.get(2)?,
            status: position_status_from_str(&row.get::<_, String>(3)?),
            quantity: Shares(row.get::<_, i64>(4)?),
            sellable_quantity: Shares(0), // 由调用方根据 lots + MarketTimeContext 派生
            avg_cost: Price(Self::parse_decimal(&row.get::<_, String>(5)?)),
            market_price: None,
            market_value: None,
            quote_freshness: None,
            realized_pnl: Money(Self::parse_decimal(&row.get::<_, String>(6)?)),
            unrealized_pnl: None,
            opened_at: parse_rfc3339(&row.get::<_, String>(7)?),
            closed_at: row.get::<_, Option<String>>(8)?.map(|s| parse_rfc3339(&s)),
            protection: None,
            actor: TradingActor::Agent,
            reasoning: row.get(9)?,
            warnings: vec![],
        })
    }

    // ------------------------------------------------------------------
    // Position lots
    // ------------------------------------------------------------------

    pub fn insert_lot(tx: &Transaction<'_>, lot: &PositionLot) -> rusqlite::Result<()> {
        tx.execute(
            "INSERT INTO account_lots
             (lot_id, position_id, ts_code, source_fill_id, trade_date,
              quantity, remaining_quantity, frozen_quantity, sellable_from, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                lot.lot_id,
                lot.position_id,
                lot.ts_code.as_str(),
                lot.source_fill_id,
                lot.trade_date.format(),
                lot.quantity.0,
                lot.remaining_quantity.0,
                lot.frozen_quantity.0,
                lot.sellable_from.format(),
                lot.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn list_lots_by_position(
        &self,
        position_id: &str,
    ) -> rusqlite::Result<Vec<PositionLot>> {
        self.db.with(|c| Self::list_lots_by_position_conn(c, position_id))
    }

    pub fn list_lots_by_position_conn(
        conn: &Connection,
        position_id: &str,
    ) -> rusqlite::Result<Vec<PositionLot>> {
        let mut stmt = conn.prepare(
            "SELECT lot_id, position_id, ts_code, source_fill_id, trade_date,
                    quantity, remaining_quantity, frozen_quantity, sellable_from, created_at
             FROM account_lots WHERE position_id = ?
             ORDER BY sellable_from ASC, created_at ASC, lot_id ASC",
        )?;
        let rows = stmt
            .query_map(params![position_id], Self::map_lot_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn update_lot_quantities(
        tx: &Transaction<'_>,
        lot_id: &str,
        remaining: Shares,
        frozen: Shares,
    ) -> rusqlite::Result<()> {
        tx.execute(
            "UPDATE account_lots SET remaining_quantity = ?, frozen_quantity = ?
             WHERE lot_id = ?",
            params![remaining.0, frozen.0, lot_id],
        )?;
        Ok(())
    }

    fn map_lot_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PositionLot> {
        let ts_code = TsCode::parse(&row.get::<_, String>(2)?)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e)))?;
        let trade_date = TradeDate::parse(&row.get::<_, String>(4)?)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e)))?;
        let sellable_from = TradeDate::parse(&row.get::<_, String>(8)?)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e)))?;
        Ok(PositionLot {
            lot_id: row.get(0)?,
            position_id: row.get(1)?,
            ts_code,
            source_fill_id: row.get(3)?,
            trade_date,
            quantity: Shares(row.get::<_, i64>(5)?),
            remaining_quantity: Shares(row.get::<_, i64>(6)?),
            frozen_quantity: Shares(row.get::<_, i64>(7)?),
            sellable_from,
            created_at: parse_rfc3339(&row.get::<_, String>(9)?),
        })
    }

    // ------------------------------------------------------------------
    // Protection
    // ------------------------------------------------------------------

    pub fn upsert_protection(
        tx: &Transaction<'_>,
        position_id: &str,
        protection: &PositionProtection,
    ) -> rusqlite::Result<()> {
        let signals_json = if protection.invalidation_signals.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&protection.invalidation_signals).unwrap_or_else(|_| "[]".into()))
        };
        tx.execute(
            "INSERT INTO account_protections
             (position_id, stop_loss, take_profit, time_stop_at, invalidation_signals,
              enabled, revision, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(position_id) DO UPDATE SET
               stop_loss = excluded.stop_loss,
               take_profit = excluded.take_profit,
               time_stop_at = excluded.time_stop_at,
               invalidation_signals = excluded.invalidation_signals,
               enabled = excluded.enabled,
               revision = excluded.revision,
               updated_at = excluded.updated_at",
            params![
                position_id,
                protection.stop_loss.map(|p| p.0.to_string()),
                protection.take_profit.map(|p| p.0.to_string()),
                protection.time_stop_at.map(|t| t.to_rfc3339()),
                signals_json,
                if protection.enabled { 1 } else { 0 },
                protection.revision,
                protection.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn get_protection(&self, position_id: &str) -> rusqlite::Result<Option<PositionProtection>> {
        self.db.with(|c| Self::get_protection_conn(c, position_id))
    }

    pub fn get_protection_conn(
        conn: &Connection,
        position_id: &str,
    ) -> rusqlite::Result<Option<PositionProtection>> {
        conn.query_row(
            "SELECT stop_loss, take_profit, time_stop_at, invalidation_signals,
                    enabled, revision, updated_at
             FROM account_protections WHERE position_id = ?",
            params![position_id],
            |row| {
                let signals: Vec<String> = match row.get::<_, Option<String>>(3)? {
                    Some(s) => serde_json::from_str(&s).unwrap_or_default(),
                    None => vec![],
                };
                Ok(PositionProtection {
                    stop_loss: row.get::<_, Option<String>>(0)?
                        .map(|s| Price(Self::parse_decimal(&s))),
                    take_profit: row.get::<_, Option<String>>(1)?
                        .map(|s| Price(Self::parse_decimal(&s))),
                    time_stop_at: row.get::<_, Option<String>>(2)?
                        .map(|s| parse_rfc3339(&s)),
                    invalidation_signals: signals,
                    enabled: row.get::<_, i64>(4)? != 0,
                    revision: row.get::<_, u32>(5)?,
                    updated_at: parse_rfc3339(&row.get::<_, String>(6)?),
                })
            },
        )
        .optional()
    }

    // ------------------------------------------------------------------
    // Watchlist
    // ------------------------------------------------------------------

    pub fn upsert_watchlist(
        tx: &Transaction<'_>,
        item: &WatchlistItem,
    ) -> rusqlite::Result<()> {
        tx.execute(
            "INSERT INTO account_watchlist (ts_code, name, note, added_at, updated_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(ts_code) DO UPDATE SET
               note = excluded.note,
               name = COALESCE(excluded.name, account_watchlist.name),
               updated_at = excluded.updated_at",
            params![
                item.ts_code.as_str(),
                item.name,
                item.note,
                item.added_at.to_rfc3339(),
                item.added_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn remove_watchlist(tx: &Transaction<'_>, ts_code: &TsCode) -> rusqlite::Result<bool> {
        let n = tx.execute(
            "DELETE FROM account_watchlist WHERE ts_code = ?",
            params![ts_code.as_str()],
        )?;
        Ok(n > 0)
    }

    pub fn get_watchlist(&self, ts_code: &TsCode) -> rusqlite::Result<Option<WatchlistItem>> {
        self.db.with(|c| {
            c.query_row(
                "SELECT ts_code, name, note, added_at FROM account_watchlist WHERE ts_code = ?",
                params![ts_code.as_str()],
                |row| {
                    let code = TsCode::parse(&row.get::<_, String>(0)?).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
                    })?;
                    Ok(WatchlistItem {
                        ts_code: code,
                        name: row.get(1)?,
                        added_at: parse_rfc3339(&row.get::<_, String>(3)?),
                        note: row.get(2)?,
                    })
                },
            )
            .optional()
        })
    }

    pub fn list_watchlist(&self) -> rusqlite::Result<Vec<WatchlistItem>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT ts_code, name, note, added_at FROM account_watchlist
                 ORDER BY added_at DESC, ts_code",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    let code = TsCode::parse(&row.get::<_, String>(0)?).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
                    })?;
                    Ok(WatchlistItem {
                        ts_code: code,
                        name: row.get(1)?,
                        added_at: parse_rfc3339(&row.get::<_, String>(3)?),
                        note: row.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    // ------------------------------------------------------------------
    // Triggers
    // ------------------------------------------------------------------

    pub fn insert_trigger_if_new(
        tx: &Transaction<'_>,
        trigger: &AccountTrigger,
    ) -> rusqlite::Result<bool> {
        let seq = Self::next_event_seq(tx)?;
        let freshness_json = trigger
            .quote_freshness
            .as_ref()
            .map(|f| serde_json::to_string(f).unwrap_or_else(|_| "null".into()));
        let warnings_json = if trigger.warnings.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&trigger.warnings).unwrap_or_else(|_| "[]".into()))
        };
        let n = tx.execute(
            "INSERT OR IGNORE INTO account_triggers
             (trigger_id, trigger_type, order_id, position_id, ts_code, price, threshold,
              quote_freshness, warnings, event_id, handled, occurred_at, seq)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                trigger.trigger_id,
                trigger.trigger_type.as_str(),
                trigger.order_id,
                trigger.position_id,
                trigger.ts_code.as_ref().map(|c| c.as_str().to_string()),
                trigger.price.map(|p| p.0.to_string()),
                trigger.threshold,
                freshness_json,
                warnings_json,
                trigger.event_id,
                if trigger.handled { 1 } else { 0 },
                trigger.occurred_at.to_rfc3339(),
                seq,
            ],
        )?;
        Ok(n > 0)
    }

    pub fn get_trigger(&self, trigger_id: &str) -> rusqlite::Result<Option<AccountTrigger>> {
        self.db.with(|c| {
            c.query_row(
                "SELECT trigger_id, trigger_type, order_id, position_id, ts_code, price,
                        threshold, quote_freshness, warnings, event_id, handled, occurred_at
                 FROM account_triggers WHERE trigger_id = ?",
                params![trigger_id],
                Self::map_trigger_row,
            )
            .optional()
        })
    }

    pub fn mark_trigger_handled(
        tx: &Transaction<'_>,
        trigger_id: &str,
    ) -> rusqlite::Result<bool> {
        let n = tx.execute(
            "UPDATE account_triggers SET handled = 1 WHERE trigger_id = ? AND handled = 0",
            params![trigger_id],
        )?;
        Ok(n > 0)
    }

    pub fn list_triggers(
        &self,
        handled: Option<bool>,
        limit: u32,
        offset: u32,
    ) -> rusqlite::Result<Vec<AccountTrigger>> {
        self.db.with(|c| {
            if let Some(h) = handled {
                let mut stmt = c.prepare(
                    "SELECT trigger_id, trigger_type, order_id, position_id, ts_code, price,
                            threshold, quote_freshness, warnings, event_id, handled, occurred_at
                     FROM account_triggers WHERE handled = ?
                     ORDER BY seq DESC LIMIT ? OFFSET ?",
                )?;
                let rows = stmt
                    .query_map(
                        params![if h { 1 } else { 0 }, limit, offset],
                        Self::map_trigger_row,
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            } else {
                let mut stmt = c.prepare(
                    "SELECT trigger_id, trigger_type, order_id, position_id, ts_code, price,
                            threshold, quote_freshness, warnings, event_id, handled, occurred_at
                     FROM account_triggers ORDER BY seq DESC LIMIT ? OFFSET ?",
                )?;
                let rows = stmt
                    .query_map(params![limit, offset], Self::map_trigger_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            }
        })
    }

    /// 同一交易日 + protection_revision + threshold 是否已生成过指定 type 的 trigger（无论 handled）。
    ///
    /// Spec: account-module.md §2 "同一 protectionRevision、同一阈值、同一交易日的价格型保护 trigger
    /// 最多生成一次，即使已 handled 也不在同日重复生成"。
    pub fn trigger_exists(tx: &Transaction<'_>, trigger_id: &str) -> rusqlite::Result<bool> {
        let n: i64 = tx.query_row(
            "SELECT COUNT(*) FROM account_triggers WHERE trigger_id = ?",
            params![trigger_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    fn map_trigger_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AccountTrigger> {
        let ts_code_str: Option<String> = row.get(4)?;
        let warnings: Vec<WarningCode> = row
            .get::<_, Option<String>>(8)?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let freshness: Option<Freshness> = row
            .get::<_, Option<String>>(7)?
            .and_then(|s| serde_json::from_str(&s).ok());
        Ok(AccountTrigger {
            trigger_id: row.get(0)?,
            trigger_type: AccountTriggerType::from_str(&row.get::<_, String>(1)?)
                .unwrap_or(AccountTriggerType::Invalidated),
            order_id: row.get(2)?,
            position_id: row.get(3)?,
            ts_code: ts_code_str.and_then(|s| TsCode::parse(&s).ok()),
            price: row.get::<_, Option<String>>(5)?
                .map(|s| Price(Self::parse_decimal(&s))),
            threshold: row.get(6)?,
            quote_freshness: freshness,
            warnings,
            event_id: row.get(9)?,
            handled: row.get::<_, i64>(10)? != 0,
            occurred_at: parse_rfc3339(&row.get::<_, String>(11)?),
        })
    }

    // ------------------------------------------------------------------
    // Freezes
    // ------------------------------------------------------------------

    pub fn upsert_freeze(tx: &Transaction<'_>, freeze: &FreezeEntry) -> rusqlite::Result<()> {
        let lots_json = if freeze.frozen_lots.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&freeze.frozen_lots).unwrap_or_else(|_| "[]".into()))
        };
        tx.execute(
            "INSERT INTO account_freezes (order_id, ts_code, side, frozen_cash, frozen_shares, frozen_lots_json)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(order_id) DO UPDATE SET
               frozen_cash = excluded.frozen_cash,
               frozen_shares = excluded.frozen_shares,
               frozen_lots_json = excluded.frozen_lots_json",
            params![
                freeze.order_id,
                freeze.ts_code.as_str(),
                order_side_str(freeze.side),
                freeze.frozen_cash.0.to_string(),
                freeze.frozen_shares.0,
                lots_json,
            ],
        )?;
        Ok(())
    }

    pub fn delete_freeze(tx: &Transaction<'_>, order_id: &str) -> rusqlite::Result<()> {
        tx.execute("DELETE FROM account_freezes WHERE order_id = ?", params![order_id])?;
        Ok(())
    }

    pub fn get_freeze(&self, order_id: &str) -> rusqlite::Result<Option<FreezeEntry>> {
        self.db.with(|c| Self::get_freeze_conn(c, order_id))
    }

    pub fn get_freeze_conn(
        conn: &Connection,
        order_id: &str,
    ) -> rusqlite::Result<Option<FreezeEntry>> {
        conn.query_row(
            "SELECT order_id, ts_code, side, frozen_cash, frozen_shares, frozen_lots_json
             FROM account_freezes WHERE order_id = ?",
            params![order_id],
            |row| {
                let ts_code = TsCode::parse(&row.get::<_, String>(1)?).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
                })?;
                let frozen_lots: Vec<FrozenLot> = row
                    .get::<_, Option<String>>(5)?
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();
                Ok(FreezeEntry {
                    order_id: row.get(0)?,
                    ts_code,
                    side: order_side_from_str(&row.get::<_, String>(2)?),
                    frozen_cash: Money(Self::parse_decimal(&row.get::<_, String>(3)?)),
                    frozen_shares: Shares(row.get::<_, i64>(4)?),
                    frozen_lots,
                })
            },
        )
        .optional()
    }

    /// 计算账户当前总冻结现金 = active orders 的 frozen_cash 之和。
    pub fn total_frozen_cash(&self) -> rusqlite::Result<Money> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT f.frozen_cash FROM account_freezes f
                 JOIN account_orders o ON o.order_id = f.order_id
                 WHERE o.status IN ('pending','partially_filled') AND f.side = 'buy'",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(Self::parse_decimal(&row.get::<_, String>(0)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let sum: Decimal = rows.into_iter().sum();
            Ok(Money(sum))
        })
    }

    /// 列出指定 ts_code 的 active buy orders（用于风控敞口计算）。
    pub fn list_active_buy_orders_for_ts_code(
        &self,
        ts_code: &TsCode,
    ) -> rusqlite::Result<Vec<Order>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT order_id, ts_code, side, order_type, limit_price, quantity,
                        filled_quantity, status, intent, position_id, reason,
                        created_at, updated_at, expires_at
                 FROM account_orders
                 WHERE ts_code = ? AND side = 'buy'
                   AND (status = 'pending'
                        OR (status = 'partially_filled' AND order_type = 'limit'))",
            )?;
            let rows = stmt
                .query_map(params![ts_code.as_str()], Self::map_order_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// 列出所有"活跃"买单 — 用于风控敞口计算。
    ///
    /// Spec: account-module.md §2 硬风控模型:
    ///   "风控敞口必须包含 active buy orders 的剩余最大占用：pending / partially_filled
    ///   买单按剩余数量和订单价格计入对应标的与总敞口"
    /// 但 `partially_filled` market 单是终态（spec §2 line 169-170），剩余量已自动取消，
    /// 不应该再计入风控敞口；只 limit partial 仍占用资金。
    pub fn list_all_active_buy_orders(&self) -> rusqlite::Result<Vec<Order>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT order_id, ts_code, side, order_type, limit_price, quantity,
                        filled_quantity, status, intent, position_id, reason,
                        created_at, updated_at, expires_at
                 FROM account_orders
                 WHERE side = 'buy'
                   AND (status = 'pending'
                        OR (status = 'partially_filled' AND order_type = 'limit'))",
            )?;
            let rows = stmt
                .query_map([], Self::map_order_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Count daily new orders for actor=agent (per Asia/Shanghai natural day).
    ///
    /// Spec: account-module.md §2 — maxDailyNewOrders.
    pub fn count_daily_new_agent_orders(
        &self,
        day_start_utc: OccurredAt,
        day_end_utc: OccurredAt,
    ) -> rusqlite::Result<u32> {
        self.db.with(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM account_orders
                 WHERE actor = 'agent'
                   AND created_at >= ? AND created_at < ?",
                params![day_start_utc.to_rfc3339(), day_end_utc.to_rfc3339()],
                |r| r.get::<_, i64>(0).map(|n| n as u32),
            )
        })
    }

    /// 列出有 protection 配置的 open positions（用于触发评估）。
    pub fn list_open_positions_with_protection(&self) -> rusqlite::Result<Vec<(Position, PositionProtection)>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT p.position_id, p.ts_code, p.name, p.status, p.quantity, p.avg_cost,
                        p.realized_pnl, p.opened_at, p.closed_at, p.reasoning
                 FROM account_positions p
                 INNER JOIN account_protections pr ON pr.position_id = p.position_id
                 WHERE p.status = 'open' AND pr.enabled = 1
                 ORDER BY p.opened_at ASC, p.position_id ASC",
            )?;
            let positions = stmt
                .query_map([], Self::map_position_row_basic)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut out = Vec::with_capacity(positions.len());
            for pos in positions {
                if let Some(prot) = Self::get_protection_conn(c, &pos.position_id)? {
                    out.push((pos, prot));
                }
            }
            Ok(out)
        })
    }

    // ------------------------------------------------------------------
    // Transaction helper
    // ------------------------------------------------------------------

    pub fn tx<R, F>(&self, f: F) -> rusqlite::Result<R>
    where
        F: FnOnce(&Transaction<'_>) -> rusqlite::Result<R>,
    {
        self.db.with(|c| {
            let tx = c.transaction()?;
            let r = f(&tx)?;
            tx.commit()?;
            Ok(r)
        })
    }
}

// ----------------------------------------------------------------------------
// Free helpers
// ----------------------------------------------------------------------------

pub fn parse_rfc3339(s: &str) -> OccurredAt {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

pub fn order_status_str(s: OrderStatus) -> &'static str {
    match s {
        OrderStatus::Pending => "pending",
        OrderStatus::PartiallyFilled => "partially_filled",
        OrderStatus::Filled => "filled",
        OrderStatus::Cancelled => "cancelled",
        OrderStatus::Rejected => "rejected",
        OrderStatus::Expired => "expired",
    }
}

pub fn order_status_from_str(s: &str) -> OrderStatus {
    match s {
        "pending" => OrderStatus::Pending,
        "partially_filled" => OrderStatus::PartiallyFilled,
        "filled" => OrderStatus::Filled,
        "cancelled" => OrderStatus::Cancelled,
        "rejected" => OrderStatus::Rejected,
        "expired" => OrderStatus::Expired,
        _ => OrderStatus::Rejected,
    }
}

pub fn order_side_str(s: OrderSide) -> &'static str {
    match s {
        OrderSide::Buy => "buy",
        OrderSide::Sell => "sell",
    }
}

pub fn order_side_from_str(s: &str) -> OrderSide {
    match s {
        "sell" => OrderSide::Sell,
        _ => OrderSide::Buy,
    }
}

pub fn order_type_str(t: OrderType) -> &'static str {
    match t {
        OrderType::Market => "market",
        OrderType::Limit => "limit",
    }
}

pub fn order_type_from_str(s: &str) -> OrderType {
    match s {
        "limit" => OrderType::Limit,
        _ => OrderType::Market,
    }
}

pub fn order_intent_str(i: OrderIntent) -> &'static str {
    match i {
        OrderIntent::OpenPosition => "open_position",
        OrderIntent::ScaleIn => "scale_in",
        OrderIntent::ScaleOut => "scale_out",
        OrderIntent::ClosePosition => "close_position",
        OrderIntent::DirectOrder => "direct_order",
    }
}

pub fn order_intent_from_str(s: &str) -> OrderIntent {
    match s {
        "open_position" => OrderIntent::OpenPosition,
        "scale_in" => OrderIntent::ScaleIn,
        "scale_out" => OrderIntent::ScaleOut,
        "close_position" => OrderIntent::ClosePosition,
        _ => OrderIntent::DirectOrder,
    }
}

pub fn position_status_str(s: PositionStatus) -> &'static str {
    match s {
        PositionStatus::Open => "open",
        PositionStatus::Closed => "closed",
    }
}

pub fn position_status_from_str(s: &str) -> PositionStatus {
    match s {
        "closed" => PositionStatus::Closed,
        _ => PositionStatus::Open,
    }
}

#[allow(dead_code)]
pub fn freshness_to_warning_status(f: &Freshness) -> Option<WarningCode> {
    match f.status {
        FreshnessStatus::Fresh => None,
        FreshnessStatus::Stale => Some(WarningCode::QuoteStale),
        FreshnessStatus::Missing => Some(WarningCode::QuoteMissing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shared::TsCode;
    use crate::infrastructure::db::run_migrations;

    fn setup() -> AppDb {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, super::super::migrations::migrations()).unwrap());
        db
    }

    #[test]
    fn insert_meta_then_read() {
        let db = setup();
        let repo = AccountRepository::new(&db);
        let cash = Money(Decimal::new(1_000_000_00, 2));
        repo.insert_meta(cash, Utc::now()).unwrap();
        let meta = repo.get_meta().unwrap().unwrap();
        assert_eq!(meta.initial_cash, cash);
        assert_eq!(meta.cash, cash);
    }

    #[test]
    fn insert_meta_is_idempotent() {
        let db = setup();
        let repo = AccountRepository::new(&db);
        let cash1 = Money(Decimal::new(100, 0));
        let cash2 = Money(Decimal::new(200, 0));
        repo.insert_meta(cash1, Utc::now()).unwrap();
        repo.insert_meta(cash2, Utc::now()).unwrap();
        let meta = repo.get_meta().unwrap().unwrap();
        // 第二次 insert 应被忽略
        assert_eq!(meta.initial_cash, cash1);
    }

    #[test]
    fn update_cash_persists() {
        let db = setup();
        let repo = AccountRepository::new(&db);
        let cash = Money(Decimal::new(100, 0));
        repo.insert_meta(cash, Utc::now()).unwrap();
        repo.update_cash(Money(Decimal::new(50, 0)), Utc::now()).unwrap();
        let meta = repo.get_meta().unwrap().unwrap();
        assert_eq!(meta.cash, Money(Decimal::new(50, 0)));
    }

    #[test]
    fn watchlist_upsert_then_list() {
        let db = setup();
        let repo = AccountRepository::new(&db);
        let code = TsCode::parse("600519.SH").unwrap();
        let item = WatchlistItem {
            ts_code: code.clone(),
            name: Some("贵州茅台".into()),
            added_at: Utc::now(),
            note: Some("hold".into()),
        };
        repo.tx(|tx| {
            AccountRepository::upsert_watchlist(tx, &item)?;
            Ok(())
        })
        .unwrap();
        let got = repo.get_watchlist(&code).unwrap().unwrap();
        assert_eq!(got.ts_code, code);
        assert_eq!(got.note.as_deref(), Some("hold"));
        let all = repo.list_watchlist().unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn watchlist_remove_returns_true_when_existed() {
        let db = setup();
        let repo = AccountRepository::new(&db);
        let code = TsCode::parse("600519.SH").unwrap();
        repo.tx(|tx| {
            AccountRepository::upsert_watchlist(
                tx,
                &WatchlistItem {
                    ts_code: code.clone(),
                    name: None,
                    added_at: Utc::now(),
                    note: None,
                },
            )?;
            Ok(())
        })
        .unwrap();
        let removed: bool = repo
            .tx(|tx| AccountRepository::remove_watchlist(tx, &code))
            .unwrap();
        assert!(removed);
        // 再次删除返回 false
        let removed2: bool = repo
            .tx(|tx| AccountRepository::remove_watchlist(tx, &code))
            .unwrap();
        assert!(!removed2);
    }

    #[test]
    fn event_seq_advances_within_tx() {
        let db = setup();
        let repo = AccountRepository::new(&db);
        let (a, b) = repo
            .tx(|tx| {
                let a = AccountRepository::next_event_seq(tx)?;
                let b = AccountRepository::next_event_seq(tx)?;
                Ok((a, b))
            })
            .unwrap();
        assert_eq!(b, a + 1);
    }

    #[test]
    fn has_account_initialized_initially_false() {
        let db = setup();
        let repo = AccountRepository::new(&db);
        assert!(!repo.has_account_initialized().unwrap());
    }

    #[test]
    fn list_lots_orders_by_sellable_from_then_created() {
        let db = setup();
        let repo = AccountRepository::new(&db);
        let code = TsCode::parse("600519.SH").unwrap();
        // Need position first
        repo.tx(|tx| {
            use crate::domain::account::types::{Position, PositionStatus, TradingActor};
            let pos = Position {
                position_id: "pos1".into(),
                ts_code: code.clone(),
                name: "x".into(),
                status: PositionStatus::Open,
                quantity: Shares(300),
                sellable_quantity: Shares(0),
                avg_cost: Price(Decimal::new(10000, 2)),
                market_price: None,
                market_value: None,
                quote_freshness: None,
                realized_pnl: Money(Decimal::ZERO),
                unrealized_pnl: None,
                opened_at: Utc::now(),
                closed_at: None,
                protection: None,
                actor: TradingActor::Agent,
                reasoning: None,
                warnings: vec![],
            };
            AccountRepository::upsert_position(tx, &pos)?;
            let lot_a = PositionLot {
                lot_id: "lot_a".into(),
                position_id: "pos1".into(),
                ts_code: code.clone(),
                source_fill_id: "f1".into(),
                trade_date: TradeDate::parse("20260520").unwrap(),
                quantity: Shares(100),
                remaining_quantity: Shares(100),
                frozen_quantity: Shares(0),
                sellable_from: TradeDate::parse("20260521").unwrap(),
                created_at: Utc::now(),
            };
            let lot_b = PositionLot {
                lot_id: "lot_b".into(),
                position_id: "pos1".into(),
                ts_code: code.clone(),
                source_fill_id: "f2".into(),
                trade_date: TradeDate::parse("20260519").unwrap(),
                quantity: Shares(200),
                remaining_quantity: Shares(200),
                frozen_quantity: Shares(0),
                sellable_from: TradeDate::parse("20260520").unwrap(),
                created_at: Utc::now(),
            };
            AccountRepository::insert_lot(tx, &lot_a)?;
            AccountRepository::insert_lot(tx, &lot_b)?;
            Ok(())
        })
        .unwrap();
        let lots = repo.list_lots_by_position("pos1").unwrap();
        // lot_b (sellable_from=20260520) 应在 lot_a (20260521) 之前
        assert_eq!(lots[0].lot_id, "lot_b");
        assert_eq!(lots[1].lot_id, "lot_a");
    }

    #[test]
    fn insert_trigger_is_idempotent_by_trigger_id() {
        let db = setup();
        let repo = AccountRepository::new(&db);
        let code = TsCode::parse("600519.SH").unwrap();
        let trig = AccountTrigger {
            trigger_id: "trg_x".into(),
            trigger_type: crate::domain::account::triggers::AccountTriggerType::StopLoss,
            order_id: None,
            position_id: Some("p1".into()),
            ts_code: Some(code.clone()),
            price: None,
            threshold: None,
            quote_freshness: None,
            warnings: vec![],
            event_id: "evt".into(),
            handled: false,
            occurred_at: Utc::now(),
        };
        let inserted_first = repo
            .tx(|tx| AccountRepository::insert_trigger_if_new(tx, &trig))
            .unwrap();
        let inserted_second = repo
            .tx(|tx| AccountRepository::insert_trigger_if_new(tx, &trig))
            .unwrap();
        assert!(inserted_first);
        assert!(!inserted_second);
    }

    #[test]
    fn mark_trigger_handled_returns_false_on_second_call() {
        let db = setup();
        let repo = AccountRepository::new(&db);
        let code = TsCode::parse("600519.SH").unwrap();
        repo.tx(|tx| {
            let trig = AccountTrigger {
                trigger_id: "trg_y".into(),
                trigger_type: crate::domain::account::triggers::AccountTriggerType::OrderFilled,
                order_id: Some("ord_1".into()),
                position_id: None,
                ts_code: Some(code.clone()),
                price: None,
                threshold: None,
                quote_freshness: None,
                warnings: vec![],
                event_id: "evt_1".into(),
                handled: false,
                occurred_at: Utc::now(),
            };
            AccountRepository::insert_trigger_if_new(tx, &trig)?;
            Ok(())
        })
        .unwrap();
        let first = repo
            .tx(|tx| AccountRepository::mark_trigger_handled(tx, "trg_y"))
            .unwrap();
        let second = repo
            .tx(|tx| AccountRepository::mark_trigger_handled(tx, "trg_y"))
            .unwrap();
        assert!(first);
        assert!(!second);
    }

    #[test]
    fn count_pending_orders_excludes_filled_and_cancelled() {
        let db = setup();
        let repo = AccountRepository::new(&db);
        let now = Utc::now();
        let code = TsCode::parse("600519.SH").unwrap();
        // 1 pending + 1 filled + 1 cancelled
        repo.tx(|tx| {
            for (i, status) in [OrderStatus::Pending, OrderStatus::Filled, OrderStatus::Cancelled]
                .iter()
                .enumerate()
            {
                use crate::domain::account::types::{OrderIntent, OrderType, TradingActor};
                let order = Order {
                    order_id: format!("ord_{}", i),
                    ts_code: code.clone(),
                    side: OrderSide::Buy,
                    order_type: OrderType::Limit,
                    limit_price: Some(Price(Decimal::new(100, 0))),
                    quantity: Shares(100),
                    filled_quantity: if matches!(status, OrderStatus::Filled) {
                        Shares(100)
                    } else {
                        Shares(0)
                    },
                    status: *status,
                    intent: OrderIntent::DirectOrder,
                    position_id: None,
                    reason: None,
                    actor: TradingActor::Agent,
                    created_at: now,
                    updated_at: now,
                    expires_at: None,
                };
                AccountRepository::upsert_order(tx, &order)?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(repo.count_pending_orders().unwrap(), 1);
    }

    #[test]
    fn protection_signals_round_trip_through_json() {
        let db = setup();
        let repo = AccountRepository::new(&db);
        let prot = PositionProtection {
            stop_loss: Some(Price(Decimal::new(8000, 2))),
            take_profit: None,
            time_stop_at: None,
            invalidation_signals: vec!["sig_a".into(), "sig_b".into()],
            enabled: true,
            revision: 3,
            updated_at: Utc::now(),
        };
        repo.tx(|tx| {
            // 需要先有一个 position 行（FK 没强制；这里直接 upsert protection）
            AccountRepository::upsert_protection(tx, "pos_x", &prot)?;
            Ok(())
        })
        .unwrap();
        let got = repo.get_protection("pos_x").unwrap().unwrap();
        assert_eq!(got.invalidation_signals, vec!["sig_a".to_string(), "sig_b".into()]);
        assert_eq!(got.revision, 3);
        assert!(got.enabled);
    }
}
