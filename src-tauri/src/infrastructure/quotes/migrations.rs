//! Quotes schema migrations。
//!
//! Spec: docs/design/quotes-module.md §2 / §3
//!
//! 表前缀 `quote_*`（AGENTS.md 分区约束）。
//!
//! 真源 / 读模型表：
//! - `quote_instruments`     — `MarketInstrument` 统一标的 universe（spec §2）
//! - `quote_klines_daily`    — 日 / 周 / 月 K 读模型 `(ts_code, period, adjust, date)`
//! - `quote_klines_minute`   — 分钟 K 读模型 `(ts_code, period, ts_ms)`
//! - `quote_intraday`        — 当日分时 `(ts_code, trade_date, time)`
//! - `quote_daily_basic`     — `DailyBasic` `(ts_code, trade_date)`
//! - `quote_company_events`  — `CompanyEvent`
//! - `quote_close_snapshot`  — 收盘快照（按 trade_date + ts_code 持久化最后行情事实）
//! - `quote_trade_calendar`  — 交易日历缓存（TuShare `trade_cal`）
//! - `quote_refresh_state`   — 收盘 refresh 完成 / 状态记录（spec §5 收盘快照完成状态）

use rusqlite_migration::M;

pub fn migrations() -> Vec<M<'static>> {
    vec![M::up(MIGRATION_001_INITIAL)]
}

const MIGRATION_001_INITIAL: &str = r#"
-- quote_instruments：MarketInstrument universe（spec §2）
CREATE TABLE quote_instruments (
    ts_code         TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    category        TEXT NOT NULL,                  -- stock / index / fund
    market          TEXT NOT NULL,                  -- SH / SZ / BJ
    board           TEXT,
    sector          TEXT,
    status          TEXT,                           -- listed / suspended / delisted / unknown
    is_st           INTEGER,
    publisher       TEXT,
    index_category  TEXT,
    fund_type       TEXT,
    management      TEXT,
    list_date       TEXT,
    source          TEXT NOT NULL,                  -- tdx / eastmoney / tushare / mixed
    updated_at      TEXT NOT NULL
);

CREATE INDEX idx_quote_instruments_category ON quote_instruments (category, ts_code);
CREATE INDEX idx_quote_instruments_name ON quote_instruments (name);

-- quote_klines_daily：日 / 周 / 月 K（spec §2）
CREATE TABLE quote_klines_daily (
    ts_code     TEXT NOT NULL,
    period      TEXT NOT NULL,    -- day / week / month
    adjust      TEXT NOT NULL,    -- none / qfq / hfq
    trade_date  TEXT NOT NULL,
    open        TEXT NOT NULL,
    close       TEXT NOT NULL,
    high        TEXT NOT NULL,
    low         TEXT NOT NULL,
    volume      INTEGER,
    amount      TEXT,
    source      TEXT NOT NULL,
    fetched_at  TEXT NOT NULL,
    PRIMARY KEY (ts_code, period, adjust, trade_date)
);

CREATE INDEX idx_quote_klines_daily_lookup ON quote_klines_daily
    (ts_code, period, adjust, trade_date DESC);

-- quote_klines_minute：分钟 K（spec §2）
CREATE TABLE quote_klines_minute (
    ts_code     TEXT NOT NULL,
    period      TEXT NOT NULL,    -- 1m / 5m / 15m / 30m / 60m
    ts_ms       INTEGER NOT NULL,
    open        TEXT NOT NULL,
    close       TEXT NOT NULL,
    high        TEXT NOT NULL,
    low         TEXT NOT NULL,
    volume      INTEGER NOT NULL,
    amount      TEXT NOT NULL,
    source      TEXT NOT NULL,
    fetched_at  TEXT NOT NULL,
    PRIMARY KEY (ts_code, period, ts_ms)
);

CREATE INDEX idx_quote_klines_minute_lookup ON quote_klines_minute
    (ts_code, period, ts_ms DESC);

-- quote_intraday：当日分时（spec §2）
CREATE TABLE quote_intraday (
    ts_code     TEXT NOT NULL,
    trade_date  TEXT NOT NULL,
    time        TEXT NOT NULL,   -- HH:mm
    price       TEXT NOT NULL,
    average     TEXT,
    volume      INTEGER,
    amount      TEXT,
    source      TEXT NOT NULL,
    fetched_at  TEXT NOT NULL,
    PRIMARY KEY (ts_code, trade_date, time)
);

-- quote_daily_basic：每日基础指标（spec §2）
CREATE TABLE quote_daily_basic (
    ts_code             TEXT NOT NULL,
    trade_date          TEXT NOT NULL,
    pe                  REAL,
    pe_ttm              REAL,
    pb                  REAL,
    ps                  REAL,
    ps_ttm              REAL,
    turnover_rate       REAL,
    turnover_rate_float REAL,
    volume_ratio        REAL,
    total_mv            TEXT,
    circ_mv             TEXT,
    source              TEXT NOT NULL,
    fetched_at          TEXT NOT NULL,
    PRIMARY KEY (ts_code, trade_date)
);

CREATE INDEX idx_quote_daily_basic_lookup ON quote_daily_basic
    (ts_code, trade_date DESC);

-- quote_company_events：公司事件（spec §2）
CREATE TABLE quote_company_events (
    id              TEXT PRIMARY KEY,
    ts_code         TEXT NOT NULL,
    event_type      TEXT NOT NULL,
    announce_date   TEXT,
    effective_date  TEXT,
    payload         TEXT NOT NULL,
    source          TEXT NOT NULL,
    fetched_at      TEXT NOT NULL
);

CREATE INDEX idx_quote_company_events_lookup ON quote_company_events
    (ts_code, effective_date, announce_date);

-- quote_close_snapshot：收盘快照（spec §5 收盘快照）
-- MARKET_SNAPSHOT 进程内 cache 不持久化；这里只持久化按 trade_date 维度的"最后行情事实"，
-- 用于非交易时段 / 启动 cache hydrate。每 (ts_code, trade_date) 一行。
CREATE TABLE quote_close_snapshot (
    ts_code     TEXT NOT NULL,
    trade_date  TEXT NOT NULL,
    payload     TEXT NOT NULL,    -- StockQuote JSON
    captured_at TEXT NOT NULL,
    source      TEXT NOT NULL,
    PRIMARY KEY (ts_code, trade_date)
);

CREATE INDEX idx_quote_close_snapshot_latest ON quote_close_snapshot
    (ts_code, trade_date DESC);

-- quote_trade_calendar：交易日历缓存（TuShare trade_cal）
CREATE TABLE quote_trade_calendar (
    cal_date   TEXT PRIMARY KEY,    -- YYYYMMDD
    is_open    INTEGER NOT NULL,    -- 0 / 1
    pretrade_date TEXT,
    source     TEXT NOT NULL,
    fetched_at TEXT NOT NULL
);

-- quote_refresh_state：collated refresh 完成状态（spec §5 收盘快照完成状态）
CREATE TABLE quote_refresh_state (
    refresh_kind  TEXT NOT NULL,        -- close / intraday / kline / daily_basic / events
    trade_date    TEXT NOT NULL,
    total         INTEGER NOT NULL,
    success       INTEGER NOT NULL,
    failed        INTEGER NOT NULL,
    completed_at  TEXT NOT NULL,
    PRIMARY KEY (refresh_kind, trade_date)
);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::db::run_migrations;
    use rusqlite::Connection;

    #[test]
    fn applies_initial_migration() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn, migrations()).unwrap();
        conn.execute(
            "INSERT INTO quote_instruments (ts_code, name, category, market, source, updated_at)
             VALUES ('600519.SH', '贵州茅台', 'stock', 'SH', 'tdx', '2026-05-26T00:00:00Z')",
            [],
        )
        .unwrap();
    }
}
