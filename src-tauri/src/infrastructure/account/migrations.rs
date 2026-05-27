//! Account schema migrations。表前缀 `account_*`。
//!
//! Spec: docs/design/account-module.md §2 / §3
//!
//! 真源表：
//! - `account_meta`             — 单账户元（initial_cash 等）。
//! - `account_events`           — append-only AccountEvent 真源（spec §2 不变量）。
//! - `account_orders`           — Order 主记录（派生自 order_placed / order_filled / 等事件）。
//! - `account_fills`            — TradeFill append-only。
//! - `account_positions`        — Position 当前 + 历史。
//! - `account_lots`             — PositionLot 派生表（T+1 / 冻结）。
//! - `account_protections`      — PositionProtection（每个 position 至多一行）。
//! - `account_watchlist`        — WatchlistItem。
//! - `account_triggers`         — AccountTrigger（trigger_id 是稳定幂等键）。
//! - `account_freezes`          — 派生表：每个 active order 的冻结现金 / 持仓金额（用于风控敞口和 snapshot 重算）。

use rusqlite_migration::M;

pub fn migrations() -> Vec<M<'static>> {
    vec![M::up(MIGRATION_001_INITIAL)]
}

const MIGRATION_001_INITIAL: &str = r#"
-- account_meta: 单账户元数据。
CREATE TABLE account_meta (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    initial_cash    TEXT NOT NULL,        -- Decimal as string
    cash            TEXT NOT NULL,        -- 当前现金（含冻结）
    initialized_at  TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

-- account_events: append-only event 真源。
CREATE TABLE account_events (
    event_id        TEXT PRIMARY KEY,
    event_type      TEXT NOT NULL,
    order_id        TEXT,
    fill_id         TEXT,
    position_id     TEXT,
    ts_code         TEXT,
    reason          TEXT,
    actor           TEXT NOT NULL,        -- agent | system | user
    payload_json    TEXT NOT NULL,
    occurred_at     TEXT NOT NULL,
    seq             INTEGER NOT NULL      -- monotonic append order tie-breaker
);

CREATE INDEX idx_account_events_seq        ON account_events (seq);
CREATE INDEX idx_account_events_occurred   ON account_events (occurred_at, seq);
CREATE INDEX idx_account_events_order      ON account_events (order_id)    WHERE order_id IS NOT NULL;
CREATE INDEX idx_account_events_position   ON account_events (position_id) WHERE position_id IS NOT NULL;
CREATE INDEX idx_account_events_ts_code    ON account_events (ts_code)     WHERE ts_code IS NOT NULL;

-- account_orders: Order 主记录（state machine 物化）。
CREATE TABLE account_orders (
    order_id          TEXT PRIMARY KEY,
    ts_code           TEXT NOT NULL,
    side              TEXT NOT NULL,         -- buy | sell
    order_type        TEXT NOT NULL,         -- market | limit
    limit_price       TEXT,                  -- Decimal as string
    quantity          INTEGER NOT NULL,
    filled_quantity   INTEGER NOT NULL DEFAULT 0,
    status            TEXT NOT NULL,         -- pending | partially_filled | filled | cancelled | rejected | expired
    intent            TEXT NOT NULL,
    position_id       TEXT,
    reason            TEXT,
    actor             TEXT NOT NULL,         -- agent
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    expires_at        TEXT
);

CREATE INDEX idx_account_orders_status    ON account_orders (status, updated_at, order_id);
CREATE INDEX idx_account_orders_ts_code   ON account_orders (ts_code, status);
CREATE INDEX idx_account_orders_pending   ON account_orders (status, expires_at)
    WHERE status IN ('pending', 'partially_filled');

-- account_fills: TradeFill append-only。
-- Spec: account-module.md §2 成交模型 — transfer_fee 仅 SH stock/fund 双向收取。
CREATE TABLE account_fills (
    fill_id       TEXT PRIMARY KEY,
    order_id      TEXT NOT NULL,
    position_id   TEXT NOT NULL,
    ts_code       TEXT NOT NULL,
    side          TEXT NOT NULL,
    price         TEXT NOT NULL,
    quantity      INTEGER NOT NULL,
    commission    TEXT NOT NULL,
    stamp_tax     TEXT NOT NULL,
    transfer_fee  TEXT NOT NULL DEFAULT '0',
    occurred_at   TEXT NOT NULL,
    seq           INTEGER NOT NULL
);

CREATE INDEX idx_account_fills_order      ON account_fills (order_id);
CREATE INDEX idx_account_fills_position   ON account_fills (position_id);
CREATE INDEX idx_account_fills_ts_code    ON account_fills (ts_code, occurred_at);

-- account_positions: Position（open + closed）。
CREATE TABLE account_positions (
    position_id     TEXT PRIMARY KEY,
    ts_code         TEXT NOT NULL,
    name            TEXT NOT NULL,
    status          TEXT NOT NULL,            -- open | closed
    quantity        INTEGER NOT NULL,
    avg_cost        TEXT NOT NULL,            -- Decimal as string
    realized_pnl    TEXT NOT NULL DEFAULT '0',
    opened_at       TEXT NOT NULL,
    closed_at       TEXT,
    actor           TEXT NOT NULL,            -- agent
    reasoning       TEXT
);

CREATE INDEX idx_account_positions_ts_code_status
    ON account_positions (ts_code, status);
CREATE INDEX idx_account_positions_status_opened
    ON account_positions (status, opened_at);

-- account_lots: PositionLot — T+1 / 冻结的最小重建单元。
CREATE TABLE account_lots (
    lot_id              TEXT PRIMARY KEY,
    position_id         TEXT NOT NULL,
    ts_code             TEXT NOT NULL,
    source_fill_id      TEXT NOT NULL,
    trade_date          TEXT NOT NULL,         -- YYYYMMDD
    quantity            INTEGER NOT NULL,
    remaining_quantity  INTEGER NOT NULL,
    frozen_quantity     INTEGER NOT NULL DEFAULT 0,
    sellable_from       TEXT NOT NULL,         -- YYYYMMDD
    created_at          TEXT NOT NULL
);

CREATE INDEX idx_account_lots_position_sellable
    ON account_lots (position_id, sellable_from, created_at, lot_id);
CREATE INDEX idx_account_lots_ts_code
    ON account_lots (ts_code);

-- account_protections: PositionProtection（仓位至多一行）。
CREATE TABLE account_protections (
    position_id          TEXT PRIMARY KEY,
    stop_loss            TEXT,                 -- Decimal as string or NULL
    take_profit          TEXT,
    time_stop_at         TEXT,
    invalidation_signals TEXT,                 -- JSON array
    enabled              INTEGER NOT NULL DEFAULT 1,
    revision             INTEGER NOT NULL DEFAULT 1,
    updated_at           TEXT NOT NULL
);

-- account_watchlist: WatchlistItem。
CREATE TABLE account_watchlist (
    ts_code   TEXT PRIMARY KEY,
    name      TEXT,
    note      TEXT,
    added_at  TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- account_triggers: 触发事件。`trigger_id` 是稳定幂等键。
CREATE TABLE account_triggers (
    trigger_id        TEXT PRIMARY KEY,
    trigger_type      TEXT NOT NULL,
    order_id          TEXT,
    position_id       TEXT,
    ts_code           TEXT,
    price             TEXT,                  -- Decimal as string
    threshold         TEXT,
    quote_freshness   TEXT,                  -- JSON
    warnings          TEXT,                  -- JSON array
    event_id          TEXT NOT NULL,
    handled           INTEGER NOT NULL DEFAULT 0,
    occurred_at       TEXT NOT NULL,
    seq               INTEGER NOT NULL
);

CREATE INDEX idx_account_triggers_handled
    ON account_triggers (handled, occurred_at, seq);
CREATE INDEX idx_account_triggers_position
    ON account_triggers (position_id)  WHERE position_id IS NOT NULL;
CREATE INDEX idx_account_triggers_order
    ON account_triggers (order_id)     WHERE order_id    IS NOT NULL;
CREATE INDEX idx_account_triggers_ts_code
    ON account_triggers (ts_code)      WHERE ts_code     IS NOT NULL;

-- account_freezes: 派生表 — 每个 active order 的冻结现金 / 持仓占用。
-- 用于：1) 计算 frozen_cash; 2) 风控敞口 active buy orders 占用; 3) cancel/expire 时释放。
CREATE TABLE account_freezes (
    order_id      TEXT PRIMARY KEY,
    ts_code       TEXT NOT NULL,
    side          TEXT NOT NULL,
    frozen_cash   TEXT NOT NULL DEFAULT '0',  -- buy order 冻结现金；sell order = 0
    frozen_shares INTEGER NOT NULL DEFAULT 0, -- sell order 冻结股数；buy order = 0
    -- 关联 sell order frozen lots: JSON array of {lotId, quantity}
    frozen_lots_json TEXT
);

CREATE INDEX idx_account_freezes_ts_code ON account_freezes (ts_code);

-- account_event_seq: 单行序列发号表（事件 append 顺序的稳定来源）。
CREATE TABLE account_event_seq (
    id      INTEGER PRIMARY KEY CHECK (id = 1),
    next    INTEGER NOT NULL
);
INSERT INTO account_event_seq (id, next) VALUES (1, 1);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::db::run_migrations;
    use rusqlite::Connection;

    #[test]
    fn applies_initial_account_schema() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn, migrations()).unwrap();
        // smoke insert
        conn.execute(
            "INSERT INTO account_meta (id, initial_cash, cash, initialized_at, updated_at)
             VALUES (1, '1000000', '1000000', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        // verify ev seq seeded
        let next: i64 = conn
            .query_row("SELECT next FROM account_event_seq WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(next, 1);
    }

    #[test]
    fn account_event_seq_advances() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn, migrations()).unwrap();
        conn.execute("UPDATE account_event_seq SET next = next + 1 WHERE id = 1", [])
            .unwrap();
        let next: i64 = conn
            .query_row("SELECT next FROM account_event_seq WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(next, 2);
    }
}
