//! Quotes SQLite repository — universe / kline / minute / intraday / daily_basic / events /
//! close_snapshot / refresh_state。
//!
//! Spec: docs/design/quotes-module.md §3 / §5

use crate::domain::quotes::quote::{CompanyEvent, CompanyEventType};
use crate::domain::quotes::{
    Adjust, DailyBasic, InstrumentSource, IntradaySeries, KlinePeriod, KlinePoint, KlineSeries,
    MarketInstrument, MinuteKlinePeriod, MinuteKlinePoint, MinuteKlineSeries, MinutePoint,
    StockQuote, XdxrCategory, XdxrEvent,
};
use crate::domain::shared::{
    Amount, Freshness, FreshnessStatus, InstrumentCategory, InstrumentStatus, Market, Money,
    OccurredAt, Percent, Price, TradeDate, TsCode, Volume,
};
use crate::infrastructure::db::AppDb;
use chrono::{DateTime, Utc};
use rusqlite::{params, types::Value as SqlValue, OptionalExtension};
use rust_decimal::{prelude::FromStr, Decimal};

pub struct QuotesRepository<'a> {
    db: &'a AppDb,
}

impl<'a> QuotesRepository<'a> {
    pub fn new(db: &'a AppDb) -> Self {
        Self { db }
    }

    // ====================================================================== instruments

    pub fn upsert_instruments(&self, items: &[MarketInstrument]) -> rusqlite::Result<()> {
        self.db.with(|conn| {
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO quote_instruments (
                        ts_code, name, category, market, board, sector, status, is_st,
                        publisher, index_category, fund_type, management, list_date,
                        source, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                     ON CONFLICT(ts_code) DO UPDATE SET
                        name = excluded.name,
                        category = excluded.category,
                        market = excluded.market,
                        board = COALESCE(excluded.board, quote_instruments.board),
                        sector = COALESCE(excluded.sector, quote_instruments.sector),
                        status = COALESCE(excluded.status, quote_instruments.status),
                        is_st = COALESCE(excluded.is_st, quote_instruments.is_st),
                        publisher = COALESCE(excluded.publisher, quote_instruments.publisher),
                        index_category = COALESCE(excluded.index_category, quote_instruments.index_category),
                        fund_type = COALESCE(excluded.fund_type, quote_instruments.fund_type),
                        management = COALESCE(excluded.management, quote_instruments.management),
                        list_date = COALESCE(excluded.list_date, quote_instruments.list_date),
                        source = excluded.source,
                        updated_at = excluded.updated_at",
                )?;
                for inst in items {
                    stmt.execute(params![
                        inst.ts_code.as_str(),
                        inst.name,
                        category_to_str(inst.category),
                        market_to_str(inst.market),
                        inst.board,
                        inst.sector,
                        inst.status.map(status_to_str),
                        inst.is_st.map(|b| if b { 1i64 } else { 0i64 }),
                        inst.publisher,
                        inst.index_category,
                        inst.fund_type,
                        inst.management,
                        inst.list_date,
                        instrument_source_to_str(inst.source),
                        inst.updated_at.to_rfc3339(),
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub fn get_instrument(&self, ts_code: &TsCode) -> rusqlite::Result<Option<MarketInstrument>> {
        self.db.with(|conn| {
            conn.query_row(
                "SELECT ts_code, name, category, market, board, sector, status, is_st,
                        publisher, index_category, fund_type, management, list_date,
                        source, updated_at
                 FROM quote_instruments WHERE ts_code = ?1",
                [ts_code.as_str()],
                row_to_instrument,
            )
            .optional()
        })
    }

    pub fn list_instruments(
        &self,
        category: Option<InstrumentCategory>,
        query: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> rusqlite::Result<(Vec<MarketInstrument>, u32)> {
        self.db.with(|conn| {
            // spec §4 list_market: query trim + 折叠连续空白
            let q = query.map(|s| {
                let trimmed = s.trim();
                let mut out = String::with_capacity(trimmed.len());
                let mut prev_ws = false;
                for ch in trimmed.chars() {
                    if ch.is_whitespace() {
                        if !prev_ws {
                            out.push(' ');
                        }
                        prev_ws = true;
                    } else {
                        out.push(ch);
                        prev_ws = false;
                    }
                }
                out
            }).filter(|s| !s.is_empty());
            let cat = category.map(category_to_str);
            // 总数
            let (cnt_sql, cnt_params): (String, Vec<SqlValue>) = match (&cat, &q) {
                (None, None) => (
                    "SELECT COUNT(*) FROM quote_instruments".to_string(),
                    vec![],
                ),
                (Some(c), None) => (
                    "SELECT COUNT(*) FROM quote_instruments WHERE category = ?1".to_string(),
                    vec![SqlValue::Text((*c).to_string())],
                ),
                (None, Some(q)) => (
                    "SELECT COUNT(*) FROM quote_instruments WHERE UPPER(ts_code) LIKE ?1 OR name LIKE ?2".to_string(),
                    vec![
                        SqlValue::Text(format!("%{}%", q.to_ascii_uppercase())),
                        SqlValue::Text(format!("%{}%", q)),
                    ],
                ),
                (Some(c), Some(q)) => (
                    "SELECT COUNT(*) FROM quote_instruments WHERE category = ?1 AND (UPPER(ts_code) LIKE ?2 OR name LIKE ?3)".to_string(),
                    vec![
                        SqlValue::Text((*c).to_string()),
                        SqlValue::Text(format!("%{}%", q.to_ascii_uppercase())),
                        SqlValue::Text(format!("%{}%", q)),
                    ],
                ),
            };
            let total: u32 = conn
                .query_row(&cnt_sql, rusqlite::params_from_iter(cnt_params.iter()), |r| {
                    r.get::<_, i64>(0)
                })?
                .try_into()
                .unwrap_or(0);

            // 列表
            let (list_sql, list_params): (String, Vec<SqlValue>) = match (&cat, &q) {
                (None, None) => (
                    "SELECT ts_code, name, category, market, board, sector, status, is_st,
                            publisher, index_category, fund_type, management, list_date,
                            source, updated_at
                     FROM quote_instruments ORDER BY ts_code LIMIT ?1 OFFSET ?2".to_string(),
                    vec![
                        SqlValue::Integer(limit as i64),
                        SqlValue::Integer(offset as i64),
                    ],
                ),
                (Some(c), None) => (
                    "SELECT ts_code, name, category, market, board, sector, status, is_st,
                            publisher, index_category, fund_type, management, list_date,
                            source, updated_at
                     FROM quote_instruments WHERE category = ?1 ORDER BY ts_code LIMIT ?2 OFFSET ?3".to_string(),
                    vec![
                        SqlValue::Text((*c).to_string()),
                        SqlValue::Integer(limit as i64),
                        SqlValue::Integer(offset as i64),
                    ],
                ),
                (None, Some(q)) => (
                    "SELECT ts_code, name, category, market, board, sector, status, is_st,
                            publisher, index_category, fund_type, management, list_date,
                            source, updated_at
                     FROM quote_instruments
                     WHERE UPPER(ts_code) LIKE ?1 OR name LIKE ?2
                     ORDER BY
                       CASE WHEN UPPER(ts_code) = ?3 THEN 0
                            WHEN name = ?4 THEN 1
                            WHEN name LIKE ?5 THEN 2
                            ELSE 3 END,
                       CASE WHEN status = 'listed' THEN 0 ELSE 1 END,
                       CASE category WHEN 'stock' THEN 0 WHEN 'index' THEN 1 WHEN 'fund' THEN 2 ELSE 3 END,
                       ts_code
                     LIMIT ?6 OFFSET ?7".to_string(),
                    vec![
                        SqlValue::Text(format!("%{}%", q.to_ascii_uppercase())),
                        SqlValue::Text(format!("%{}%", q)),
                        SqlValue::Text(q.to_ascii_uppercase()),
                        SqlValue::Text(q.clone()),
                        SqlValue::Text(format!("{}%", q)),
                        SqlValue::Integer(limit as i64),
                        SqlValue::Integer(offset as i64),
                    ],
                ),
                (Some(c), Some(q)) => (
                    "SELECT ts_code, name, category, market, board, sector, status, is_st,
                            publisher, index_category, fund_type, management, list_date,
                            source, updated_at
                     FROM quote_instruments
                     WHERE category = ?1 AND (UPPER(ts_code) LIKE ?2 OR name LIKE ?3)
                     ORDER BY
                       CASE WHEN UPPER(ts_code) = ?4 THEN 0
                            WHEN name = ?5 THEN 1
                            WHEN name LIKE ?6 THEN 2
                            ELSE 3 END,
                       CASE WHEN status = 'listed' THEN 0 ELSE 1 END,
                       ts_code
                     LIMIT ?7 OFFSET ?8".to_string(),
                    vec![
                        SqlValue::Text((*c).to_string()),
                        SqlValue::Text(format!("%{}%", q.to_ascii_uppercase())),
                        SqlValue::Text(format!("%{}%", q)),
                        SqlValue::Text(q.to_ascii_uppercase()),
                        SqlValue::Text(q.clone()),
                        SqlValue::Text(format!("{}%", q)),
                        SqlValue::Integer(limit as i64),
                        SqlValue::Integer(offset as i64),
                    ],
                ),
            };
            let mut stmt = conn.prepare(&list_sql)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(list_params.iter()), row_to_instrument)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((rows, total))
        })
    }

    // ====================================================================== klines (daily)

    pub fn upsert_daily_klines(
        &self,
        ts_code: &TsCode,
        period: KlinePeriod,
        adjust: Adjust,
        points: &[KlinePoint],
        source: &str,
        fetched_at: OccurredAt,
    ) -> rusqlite::Result<()> {
        self.db.with(|conn| {
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO quote_klines_daily (
                        ts_code, period, adjust, trade_date, open, close, high, low,
                        volume, amount, source, fetched_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                     ON CONFLICT(ts_code, period, adjust, trade_date) DO UPDATE SET
                        open = excluded.open,
                        close = excluded.close,
                        high = excluded.high,
                        low = excluded.low,
                        volume = excluded.volume,
                        amount = excluded.amount,
                        source = excluded.source,
                        fetched_at = excluded.fetched_at",
                )?;
                for p in points {
                    stmt.execute(params![
                        ts_code.as_str(),
                        period.as_str(),
                        adjust.as_str(),
                        p.date.format(),
                        p.open.0.to_string(),
                        p.close.0.to_string(),
                        p.high.0.to_string(),
                        p.low.0.to_string(),
                        p.volume.map(|v| v.0),
                        p.amount.map(|a| a.0.to_string()),
                        source,
                        fetched_at.to_rfc3339(),
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub fn load_kline_series(
        &self,
        ts_code: &TsCode,
        period: KlinePeriod,
        adjust: Adjust,
        limit: u32,
    ) -> rusqlite::Result<Option<KlineSeries>> {
        let (points, fetched_at, source) = self.db.with(|conn| -> rusqlite::Result<_> {
            let mut stmt = conn.prepare(
                "SELECT trade_date, open, close, high, low, volume, amount, source, fetched_at
                 FROM quote_klines_daily
                 WHERE ts_code = ?1 AND period = ?2 AND adjust = ?3
                 ORDER BY trade_date DESC LIMIT ?4",
            )?;
            let mut points: Vec<KlinePoint> = Vec::new();
            let mut latest_fetched: Option<DateTime<Utc>> = None;
            let mut latest_source: Option<String> = None;
            let mut rows = stmt.query(params![
                ts_code.as_str(),
                period.as_str(),
                adjust.as_str(),
                limit as i64
            ])?;
            while let Some(row) = rows.next()? {
                let td: String = row.get(0)?;
                let open: String = row.get(1)?;
                let close: String = row.get(2)?;
                let high: String = row.get(3)?;
                let low: String = row.get(4)?;
                let volume: Option<i64> = row.get(5)?;
                let amount: Option<String> = row.get(6)?;
                let source: String = row.get(7)?;
                let fetched: String = row.get(8)?;
                let p = KlinePoint {
                    date: TradeDate::parse(&td).map_err(|_| rusqlite::Error::InvalidQuery)?,
                    open: Price(Decimal::from_str(&open).unwrap_or_default()),
                    close: Price(Decimal::from_str(&close).unwrap_or_default()),
                    high: Price(Decimal::from_str(&high).unwrap_or_default()),
                    low: Price(Decimal::from_str(&low).unwrap_or_default()),
                    volume: volume.map(Volume),
                    amount: amount
                        .and_then(|s| Decimal::from_str(&s).ok())
                        .map(Amount),
                };
                points.push(p);
                if latest_fetched.is_none() {
                    latest_fetched = DateTime::parse_from_rfc3339(&fetched)
                        .ok()
                        .map(|d| d.with_timezone(&Utc));
                    latest_source = Some(source);
                }
            }
            Ok((points, latest_fetched, latest_source))
        })?;
        if points.is_empty() {
            return Ok(None);
        }
        let mut sorted = points;
        sorted.reverse(); // ascending
        Ok(Some(KlineSeries {
            period,
            adjust,
            points: sorted,
            freshness: Freshness {
                status: FreshnessStatus::Fresh,
                captured_at: fetched_at,
                exchange_time: None,
                age_ms: fetched_at.map(|t| (Utc::now() - t).num_milliseconds()),
                source: source,
                warning: None,
            },
            warnings: Vec::new(),
        }))
    }

    /// 查 `quote_klines_daily` 中 `(ts_code, period, adjust = none)` 的最新 trade_date。
    /// 用于 incremental refresh：只从 `max+1` 开始往后补。
    ///
    /// Spec: docs/design/quotes-module.md §5 "增量 K 线"。
    pub fn max_kline_trade_date(
        &self,
        ts_code: &TsCode,
        period: KlinePeriod,
    ) -> rusqlite::Result<Option<TradeDate>> {
        self.db.with(|conn| {
            let res: Option<String> = conn
                .query_row(
                    "SELECT MAX(trade_date) FROM quote_klines_daily
                     WHERE ts_code = ?1 AND period = ?2 AND adjust = 'none'",
                    params![ts_code.as_str(), period.as_str()],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()
                .map(|o| o.flatten())?;
            Ok(res.and_then(|s| TradeDate::parse(&s).ok()))
        })
    }

    // ====================================================================== klines (minute)

    pub fn upsert_minute_klines(
        &self,
        ts_code: &TsCode,
        period: MinuteKlinePeriod,
        points: &[MinuteKlinePoint],
        source: &str,
        fetched_at: OccurredAt,
    ) -> rusqlite::Result<()> {
        self.db.with(|conn| {
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO quote_klines_minute (
                        ts_code, period, ts_ms, open, close, high, low, volume, amount,
                        source, fetched_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                     ON CONFLICT(ts_code, period, ts_ms) DO UPDATE SET
                        open = excluded.open,
                        close = excluded.close,
                        high = excluded.high,
                        low = excluded.low,
                        volume = excluded.volume,
                        amount = excluded.amount,
                        source = excluded.source,
                        fetched_at = excluded.fetched_at",
                )?;
                for p in points {
                    stmt.execute(params![
                        ts_code.as_str(),
                        period.as_str(),
                        p.timestamp_ms,
                        p.open.0.to_string(),
                        p.close.0.to_string(),
                        p.high.0.to_string(),
                        p.low.0.to_string(),
                        p.volume.0,
                        p.amount.0.to_string(),
                        source,
                        fetched_at.to_rfc3339(),
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub fn load_minute_series(
        &self,
        ts_code: &TsCode,
        period: MinuteKlinePeriod,
        limit: u32,
    ) -> rusqlite::Result<Option<MinuteKlineSeries>> {
        let (points, fetched_at, source) = self.db.with(|conn| -> rusqlite::Result<_> {
            let mut stmt = conn.prepare(
                "SELECT ts_ms, open, close, high, low, volume, amount, source, fetched_at
                 FROM quote_klines_minute
                 WHERE ts_code = ?1 AND period = ?2
                 ORDER BY ts_ms DESC LIMIT ?3",
            )?;
            let mut points: Vec<MinuteKlinePoint> = Vec::new();
            let mut latest_fetched: Option<DateTime<Utc>> = None;
            let mut latest_source: Option<String> = None;
            let mut rows = stmt.query(params![
                ts_code.as_str(),
                period.as_str(),
                limit as i64
            ])?;
            while let Some(row) = rows.next()? {
                let ts_ms: i64 = row.get(0)?;
                let open: String = row.get(1)?;
                let close: String = row.get(2)?;
                let high: String = row.get(3)?;
                let low: String = row.get(4)?;
                let volume: i64 = row.get(5)?;
                let amount: String = row.get(6)?;
                let src: String = row.get(7)?;
                let fetched: String = row.get(8)?;
                points.push(MinuteKlinePoint {
                    timestamp_ms: ts_ms,
                    open: Price(Decimal::from_str(&open).unwrap_or_default()),
                    close: Price(Decimal::from_str(&close).unwrap_or_default()),
                    high: Price(Decimal::from_str(&high).unwrap_or_default()),
                    low: Price(Decimal::from_str(&low).unwrap_or_default()),
                    volume: Volume(volume),
                    amount: Amount(Decimal::from_str(&amount).unwrap_or_default()),
                });
                if latest_fetched.is_none() {
                    latest_fetched = DateTime::parse_from_rfc3339(&fetched)
                        .ok()
                        .map(|d| d.with_timezone(&Utc));
                    latest_source = Some(src);
                }
            }
            Ok((points, latest_fetched, latest_source))
        })?;
        if points.is_empty() {
            return Ok(None);
        }
        let mut sorted = points;
        sorted.reverse();
        Ok(Some(MinuteKlineSeries {
            period,
            points: sorted,
            freshness: Freshness {
                status: FreshnessStatus::Fresh,
                captured_at: fetched_at,
                exchange_time: None,
                age_ms: fetched_at.map(|t| (Utc::now() - t).num_milliseconds()),
                source: source,
                warning: None,
            },
            warnings: Vec::new(),
        }))
    }

    // ====================================================================== intraday

    pub fn upsert_intraday(
        &self,
        ts_code: &TsCode,
        trade_date: TradeDate,
        points: &[(String, Price, Option<Volume>, Option<Amount>)],
        source: &str,
        fetched_at: OccurredAt,
    ) -> rusqlite::Result<()> {
        self.db.with(|conn| {
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO quote_intraday (
                        ts_code, trade_date, time, price, average, volume, amount,
                        source, fetched_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(ts_code, trade_date, time) DO UPDATE SET
                        price = excluded.price,
                        volume = excluded.volume,
                        amount = excluded.amount,
                        source = excluded.source,
                        fetched_at = excluded.fetched_at",
                )?;
                for (time, price, volume, amount) in points {
                    stmt.execute(params![
                        ts_code.as_str(),
                        trade_date.format(),
                        time,
                        price.0.to_string(),
                        Option::<String>::None,
                        volume.map(|v| v.0),
                        amount.map(|a| a.0.to_string()),
                        source,
                        fetched_at.to_rfc3339(),
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub fn load_intraday(
        &self,
        ts_code: &TsCode,
        trade_date: TradeDate,
    ) -> rusqlite::Result<Option<IntradaySeries>> {
        let (points, fetched_at, source) = self.db.with(|conn| -> rusqlite::Result<_> {
            let mut stmt = conn.prepare(
                "SELECT time, price, average, volume, amount, source, fetched_at
                 FROM quote_intraday
                 WHERE ts_code = ?1 AND trade_date = ?2
                 ORDER BY time ASC",
            )?;
            let mut points: Vec<MinutePoint> = Vec::new();
            let mut latest_fetched: Option<DateTime<Utc>> = None;
            let mut latest_source: Option<String> = None;
            let mut rows =
                stmt.query(params![ts_code.as_str(), trade_date.format()])?;
            while let Some(row) = rows.next()? {
                let time: String = row.get(0)?;
                let price: String = row.get(1)?;
                let avg: Option<String> = row.get(2)?;
                let volume: Option<i64> = row.get(3)?;
                let amount: Option<String> = row.get(4)?;
                let src: String = row.get(5)?;
                let fetched: String = row.get(6)?;
                points.push(MinutePoint {
                    trade_date,
                    time,
                    price: Price(Decimal::from_str(&price).unwrap_or_default()),
                    average: avg.and_then(|s| Decimal::from_str(&s).ok()).map(Price),
                    volume: volume.map(Volume),
                    amount: amount.and_then(|s| Decimal::from_str(&s).ok()).map(Amount),
                });
                if latest_fetched.is_none() {
                    latest_fetched = DateTime::parse_from_rfc3339(&fetched)
                        .ok()
                        .map(|d| d.with_timezone(&Utc));
                    latest_source = Some(src);
                }
            }
            Ok((points, latest_fetched, latest_source))
        })?;
        if points.is_empty() {
            return Ok(None);
        }
        Ok(Some(IntradaySeries {
            trade_date,
            points,
            freshness: Freshness {
                status: FreshnessStatus::Fresh,
                captured_at: fetched_at,
                exchange_time: None,
                age_ms: fetched_at.map(|t| (Utc::now() - t).num_milliseconds()),
                source: source,
                warning: None,
            },
            warnings: Vec::new(),
        }))
    }

    // ====================================================================== daily_basic

    pub fn upsert_daily_basic(&self, rows: &[DailyBasic]) -> rusqlite::Result<()> {
        self.db.with(|conn| {
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO quote_daily_basic (
                        ts_code, trade_date, pe, pe_ttm, pb, ps, ps_ttm,
                        turnover_rate, turnover_rate_float, volume_ratio,
                        total_mv, circ_mv, source, fetched_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                     ON CONFLICT(ts_code, trade_date) DO UPDATE SET
                        pe = excluded.pe, pe_ttm = excluded.pe_ttm,
                        pb = excluded.pb, ps = excluded.ps, ps_ttm = excluded.ps_ttm,
                        turnover_rate = excluded.turnover_rate,
                        turnover_rate_float = excluded.turnover_rate_float,
                        volume_ratio = excluded.volume_ratio,
                        total_mv = excluded.total_mv,
                        circ_mv = excluded.circ_mv,
                        source = excluded.source,
                        fetched_at = excluded.fetched_at",
                )?;
                for d in rows {
                    stmt.execute(params![
                        d.ts_code.as_str(),
                        d.trade_date.format(),
                        d.pe,
                        d.pe_ttm,
                        d.pb,
                        d.ps,
                        d.ps_ttm,
                        d.turnover_rate,
                        d.turnover_rate_float,
                        d.volume_ratio,
                        d.total_mv.map(|m| m.0.to_string()),
                        d.circ_mv.map(|m| m.0.to_string()),
                        d.source,
                        d.fetched_at.to_rfc3339(),
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// 取 `<= trade_date` 的最新一条 DailyBasic（spec §2：扫描使用最近一条）。
    pub fn latest_daily_basic(
        &self,
        ts_code: &TsCode,
        trade_date: TradeDate,
    ) -> rusqlite::Result<Option<DailyBasic>> {
        self.db.with(|conn| {
            conn.query_row(
                "SELECT ts_code, trade_date, pe, pe_ttm, pb, ps, ps_ttm,
                        turnover_rate, turnover_rate_float, volume_ratio,
                        total_mv, circ_mv, source, fetched_at
                 FROM quote_daily_basic
                 WHERE ts_code = ?1 AND trade_date <= ?2
                 ORDER BY trade_date DESC LIMIT 1",
                params![ts_code.as_str(), trade_date.format()],
                row_to_daily_basic,
            )
            .optional()
        })
    }

    // ====================================================================== company events

    pub fn upsert_company_events(&self, events: &[CompanyEvent]) -> rusqlite::Result<()> {
        self.db.with(|conn| {
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO quote_company_events (
                        id, ts_code, event_type, announce_date, effective_date,
                        payload, source, fetched_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(id) DO UPDATE SET
                        announce_date = excluded.announce_date,
                        effective_date = excluded.effective_date,
                        payload = excluded.payload,
                        source = excluded.source,
                        fetched_at = excluded.fetched_at",
                )?;
                for ev in events {
                    stmt.execute(params![
                        ev.id,
                        ev.ts_code.as_str(),
                        event_type_to_str(ev.event_type),
                        ev.announce_date.map(|d| d.format()),
                        ev.effective_date.map(|d| d.format()),
                        ev.payload.to_string(),
                        ev.source,
                        ev.fetched_at.to_rfc3339(),
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub fn list_company_events(
        &self,
        ts_code: &TsCode,
        days_ahead: i64,
    ) -> rusqlite::Result<Vec<CompanyEvent>> {
        let today = chrono::Utc::now().date_naive();
        let from = TradeDate::from_naive(today)
            .as_naive()
            .pred_opt()
            .unwrap()
            .format("%Y%m%d")
            .to_string();
        let to = TradeDate::from_naive(
            today.checked_add_days(chrono::Days::new(days_ahead as u64)).unwrap_or(today),
        )
        .format();
        self.db.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, ts_code, event_type, announce_date, effective_date,
                        payload, source, fetched_at
                 FROM quote_company_events
                 WHERE ts_code = ?1
                   AND ( (effective_date IS NOT NULL AND effective_date BETWEEN ?2 AND ?3)
                      OR (announce_date IS NOT NULL AND announce_date BETWEEN ?2 AND ?3) )
                 ORDER BY COALESCE(effective_date, announce_date) DESC",
            )?;
            let rows = stmt
                .query_map(params![ts_code.as_str(), from, to], row_to_event)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    // ====================================================================== close snapshot

    pub fn upsert_close_snapshot(
        &self,
        ts_code: &TsCode,
        trade_date: TradeDate,
        quote: &StockQuote,
    ) -> rusqlite::Result<()> {
        let json = serde_json::to_string(quote).unwrap_or_else(|_| "{}".to_string());
        self.db.with(|conn| {
            conn.execute(
                "INSERT INTO quote_close_snapshot (ts_code, trade_date, payload, captured_at, source)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(ts_code, trade_date) DO UPDATE SET
                    payload = excluded.payload,
                    captured_at = excluded.captured_at,
                    source = excluded.source",
                params![
                    ts_code.as_str(),
                    trade_date.format(),
                    json,
                    quote.captured_at.to_rfc3339(),
                    quote.source.as_str(),
                ],
            )?;
            Ok(())
        })
    }

    pub fn load_close_snapshot(
        &self,
        ts_code: &TsCode,
        trade_date: TradeDate,
    ) -> rusqlite::Result<Option<StockQuote>> {
        self.db.with(|conn| {
            let payload: Option<String> = conn
                .query_row(
                    "SELECT payload FROM quote_close_snapshot WHERE ts_code = ?1 AND trade_date = ?2",
                    params![ts_code.as_str(), trade_date.format()],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            Ok(payload.and_then(|s| serde_json::from_str(&s).ok()))
        })
    }

    // ====================================================================== refresh_state

    /// 读取最近一次指定 kind / trade_date 的 refresh 状态。
    pub fn read_refresh_state(
        &self,
        kind: &str,
        trade_date: TradeDate,
    ) -> rusqlite::Result<Option<(u32, u32, u32, OccurredAt)>> {
        self.db.with(|conn| {
            conn.query_row(
                "SELECT total, success, failed, completed_at FROM quote_refresh_state
                 WHERE refresh_kind = ?1 AND trade_date = ?2",
                params![kind, trade_date.format()],
                |r| {
                    let total: i64 = r.get(0)?;
                    let success: i64 = r.get(1)?;
                    let failed: i64 = r.get(2)?;
                    let completed_at: String = r.get(3)?;
                    Ok((
                        total as u32,
                        success as u32,
                        failed as u32,
                        DateTime::parse_from_rfc3339(&completed_at)
                            .map(|d| d.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                    ))
                },
            )
            .optional()
        })
    }

    /// 取所有 instruments 的 `(ts_code, category)` map（用于 snapshot cache 类别一致性校验）。
    pub fn instrument_category_map(
        &self,
    ) -> rusqlite::Result<std::collections::HashMap<TsCode, InstrumentCategory>> {
        self.db.with(|conn| {
            let mut stmt = conn.prepare("SELECT ts_code, category FROM quote_instruments")?;
            let rows = stmt
                .query_map([], |r| {
                    let ts: String = r.get(0)?;
                    let cat: String = r.get(1)?;
                    Ok((ts, cat))
                })?
                .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
            let mut out = std::collections::HashMap::with_capacity(rows.len());
            for (ts, cat) in rows {
                if let Ok(code) = TsCode::parse(&ts) {
                    out.insert(code, category_from_str(&cat));
                }
            }
            Ok(out)
        })
    }

    /// 仅读取 universe 的 ts_codes 子集（按 status='listed' 过滤；spec §2 — universe 不包含
    /// delisted 标的）。
    pub fn list_universe_ts_codes(&self) -> rusqlite::Result<Vec<TsCode>> {
        self.db.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT ts_code FROM quote_instruments
                 WHERE status IS NULL OR status = 'listed'
                 ORDER BY ts_code",
            )?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(rows
                .into_iter()
                .filter_map(|s| TsCode::parse(&s).ok())
                .collect())
        })
    }

    pub fn record_refresh_state(
        &self,
        kind: &str,
        trade_date: TradeDate,
        total: u32,
        success: u32,
        failed: u32,
        completed_at: OccurredAt,
    ) -> rusqlite::Result<()> {
        self.db.with(|conn| {
            conn.execute(
                "INSERT INTO quote_refresh_state (refresh_kind, trade_date, total, success, failed, completed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(refresh_kind, trade_date) DO UPDATE SET
                    total = excluded.total,
                    success = excluded.success,
                    failed = excluded.failed,
                    completed_at = excluded.completed_at",
                params![
                    kind,
                    trade_date.format(),
                    total as i64,
                    success as i64,
                    failed as i64,
                    completed_at.to_rfc3339(),
                ],
            )?;
            Ok(())
        })
    }

    // ====================================================================== xdxr_events
    //
    // Spec: quotes-module.md §2 "本地复权计算（基于 TDX xdxr）"

    /// Upsert xdxr events for a ts_code. Idempotent by (ts_code, occur_date, category).
    pub fn upsert_xdxr_events(
        &self,
        ts_code: &TsCode,
        events: &[XdxrEvent],
    ) -> rusqlite::Result<usize> {
        self.db.with(|conn| {
            let tx = conn.transaction()?;
            let mut written = 0usize;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO quote_xdxr_events (
                        ts_code, occur_date, category,
                        fenhong, peigujia, songzhuangu, peigu,
                        suogu, xingquanjia, fenshu,
                        panqianliutong, qianzongguben, panhouliutong, houzongguben,
                        fetched_at, source
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                     ON CONFLICT(ts_code, occur_date, category) DO UPDATE SET
                        fenhong = excluded.fenhong,
                        peigujia = excluded.peigujia,
                        songzhuangu = excluded.songzhuangu,
                        peigu = excluded.peigu,
                        suogu = excluded.suogu,
                        xingquanjia = excluded.xingquanjia,
                        fenshu = excluded.fenshu,
                        panqianliutong = excluded.panqianliutong,
                        qianzongguben = excluded.qianzongguben,
                        panhouliutong = excluded.panhouliutong,
                        houzongguben = excluded.houzongguben,
                        fetched_at = excluded.fetched_at,
                        source = excluded.source",
                )?;
                for e in events {
                    if e.ts_code.as_str() != ts_code.as_str() {
                        // skip mismatched ts_code (defensive — caller should pre-filter)
                        continue;
                    }
                    stmt.execute(params![
                        e.ts_code.as_str(),
                        e.occur_date.format(),
                        e.category.as_u8() as i64,
                        e.fenhong,
                        e.peigujia,
                        e.songzhuangu,
                        e.peigu,
                        e.suogu,
                        e.xingquanjia,
                        e.fenshu,
                        e.panqianliutong,
                        e.qianzongguben,
                        e.panhouliutong,
                        e.houzongguben,
                        e.fetched_at,
                        "tdx",
                    ])?;
                    written += 1;
                }
            }
            tx.commit()?;
            Ok(written)
        })
    }

    /// List all xdxr events for a ts_code, ordered by `occur_date ASC, category ASC`。
    pub fn list_xdxr_events(&self, ts_code: &TsCode) -> rusqlite::Result<Vec<XdxrEvent>> {
        self.db.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT ts_code, occur_date, category,
                        fenhong, peigujia, songzhuangu, peigu,
                        suogu, xingquanjia, fenshu,
                        panqianliutong, qianzongguben, panhouliutong, houzongguben,
                        fetched_at
                 FROM quote_xdxr_events
                 WHERE ts_code = ?1
                 ORDER BY occur_date ASC, category ASC",
            )?;
            let mut rows = stmt.query(params![ts_code.as_str()])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                if let Ok(ev) = row_to_xdxr(row) {
                    out.push(ev);
                }
            }
            Ok(out)
        })
    }

    /// Delete all xdxr events for a ts_code（用于刷新前清空，保证幂等重建）。
    pub fn delete_xdxr_events(&self, ts_code: &TsCode) -> rusqlite::Result<usize> {
        self.db.with(|conn| {
            let n = conn.execute(
                "DELETE FROM quote_xdxr_events WHERE ts_code = ?1",
                params![ts_code.as_str()],
            )?;
            Ok(n)
        })
    }
}

fn row_to_xdxr(row: &rusqlite::Row<'_>) -> rusqlite::Result<XdxrEvent> {
    let ts_code_s: String = row.get(0)?;
    let ts_code = TsCode::parse(&ts_code_s).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let occur_date_s: String = row.get(1)?;
    let occur_date = TradeDate::parse(&occur_date_s).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let category_raw: i64 = row.get(2)?;
    let category =
        XdxrCategory::from_u8(category_raw as u8).ok_or(rusqlite::Error::InvalidQuery)?;
    Ok(XdxrEvent {
        ts_code,
        occur_date,
        category,
        fenhong: row.get(3)?,
        peigujia: row.get(4)?,
        songzhuangu: row.get(5)?,
        peigu: row.get(6)?,
        suogu: row.get(7)?,
        xingquanjia: row.get(8)?,
        fenshu: row.get(9)?,
        panqianliutong: row.get(10)?,
        qianzongguben: row.get(11)?,
        panhouliutong: row.get(12)?,
        houzongguben: row.get(13)?,
        fetched_at: row.get(14)?,
    })
}

// ---------------------------------------------------------------- helpers

fn category_to_str(c: InstrumentCategory) -> &'static str {
    match c {
        InstrumentCategory::Stock => "stock",
        InstrumentCategory::Index => "index",
        InstrumentCategory::Fund => "fund",
    }
}

fn category_from_str(s: &str) -> InstrumentCategory {
    match s {
        "index" => InstrumentCategory::Index,
        "fund" => InstrumentCategory::Fund,
        _ => InstrumentCategory::Stock,
    }
}

fn market_to_str(m: Market) -> &'static str {
    match m {
        Market::SH => "SH",
        Market::SZ => "SZ",
        Market::BJ => "BJ",
    }
}

fn market_from_str(s: &str) -> Market {
    match s {
        "SZ" => Market::SZ,
        "BJ" => Market::BJ,
        _ => Market::SH,
    }
}

fn status_to_str(s: InstrumentStatus) -> &'static str {
    match s {
        InstrumentStatus::Listed => "listed",
        InstrumentStatus::Suspended => "suspended",
        InstrumentStatus::Delisted => "delisted",
        InstrumentStatus::Unknown => "unknown",
    }
}

fn status_from_str(s: &str) -> InstrumentStatus {
    match s {
        "suspended" => InstrumentStatus::Suspended,
        "delisted" => InstrumentStatus::Delisted,
        "unknown" => InstrumentStatus::Unknown,
        _ => InstrumentStatus::Listed,
    }
}

fn instrument_source_to_str(s: InstrumentSource) -> &'static str {
    match s {
        InstrumentSource::Tdx => "tdx",
        InstrumentSource::Eastmoney => "eastmoney",
        InstrumentSource::Tushare => "tushare",
        InstrumentSource::Mixed => "mixed",
    }
}

fn instrument_source_from_str(s: &str) -> InstrumentSource {
    match s {
        "eastmoney" => InstrumentSource::Eastmoney,
        "tushare" => InstrumentSource::Tushare,
        "mixed" => InstrumentSource::Mixed,
        _ => InstrumentSource::Tdx,
    }
}

fn event_type_to_str(t: CompanyEventType) -> &'static str {
    match t {
        CompanyEventType::Dividend => "dividend",
        CompanyEventType::Suspension => "suspension",
        CompanyEventType::Resume => "resume",
        CompanyEventType::St => "st",
        CompanyEventType::EarningsForecast => "earnings_forecast",
        CompanyEventType::Unlock => "unlock",
        CompanyEventType::Other => "other",
    }
}

fn event_type_from_str(s: &str) -> CompanyEventType {
    match s {
        "dividend" => CompanyEventType::Dividend,
        "suspension" => CompanyEventType::Suspension,
        "resume" => CompanyEventType::Resume,
        "st" => CompanyEventType::St,
        "earnings_forecast" => CompanyEventType::EarningsForecast,
        "unlock" => CompanyEventType::Unlock,
        _ => CompanyEventType::Other,
    }
}

fn row_to_instrument(row: &rusqlite::Row<'_>) -> rusqlite::Result<MarketInstrument> {
    let ts_code: String = row.get(0)?;
    let ts_code = TsCode::parse(&ts_code).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let name: String = row.get(1)?;
    let category: String = row.get(2)?;
    let market: String = row.get(3)?;
    let board: Option<String> = row.get(4)?;
    let sector: Option<String> = row.get(5)?;
    let status: Option<String> = row.get(6)?;
    let is_st: Option<i64> = row.get(7)?;
    let publisher: Option<String> = row.get(8)?;
    let index_category: Option<String> = row.get(9)?;
    let fund_type: Option<String> = row.get(10)?;
    let management: Option<String> = row.get(11)?;
    let list_date: Option<String> = row.get(12)?;
    let source: String = row.get(13)?;
    let updated_at: String = row.get(14)?;
    let updated_at = DateTime::parse_from_rfc3339(&updated_at)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    Ok(MarketInstrument {
        ts_code,
        name,
        category: category_from_str(&category),
        market: market_from_str(&market),
        board,
        sector,
        status: status.as_deref().map(status_from_str),
        is_st: is_st.map(|v| v != 0),
        publisher,
        index_category,
        fund_type,
        management,
        list_date,
        source: instrument_source_from_str(&source),
        updated_at,
    })
}

fn row_to_daily_basic(row: &rusqlite::Row<'_>) -> rusqlite::Result<DailyBasic> {
    let ts_code: String = row.get(0)?;
    let ts_code = TsCode::parse(&ts_code).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let trade_date: String = row.get(1)?;
    let trade_date = TradeDate::parse(&trade_date).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let pe: Option<f64> = row.get(2)?;
    let pe_ttm: Option<f64> = row.get(3)?;
    let pb: Option<f64> = row.get(4)?;
    let ps: Option<f64> = row.get(5)?;
    let ps_ttm: Option<f64> = row.get(6)?;
    let turnover_rate: Option<f64> = row.get(7)?;
    let turnover_rate_float: Option<f64> = row.get(8)?;
    let volume_ratio: Option<f64> = row.get(9)?;
    let total_mv: Option<String> = row.get(10)?;
    let circ_mv: Option<String> = row.get(11)?;
    let source: String = row.get(12)?;
    let fetched_at: String = row.get(13)?;
    let fetched_at = DateTime::parse_from_rfc3339(&fetched_at)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    Ok(DailyBasic {
        ts_code,
        trade_date,
        pe,
        pe_ttm,
        pb,
        ps,
        ps_ttm,
        turnover_rate: turnover_rate.map(|v| v as Percent),
        turnover_rate_float: turnover_rate_float.map(|v| v as Percent),
        volume_ratio,
        total_mv: total_mv
            .and_then(|s| Decimal::from_str(&s).ok())
            .map(Money),
        circ_mv: circ_mv.and_then(|s| Decimal::from_str(&s).ok()).map(Money),
        source,
        fetched_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::quotes::{InstrumentSource, MarketInstrument};
    use crate::infrastructure::db::run_migrations;
    use crate::infrastructure::quotes::migrations as quotes_migrations;
    use chrono::Utc;

    fn make_db() -> AppDb {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|conn| run_migrations(conn, quotes_migrations()).unwrap());
        db
    }

    fn inst(ts: &str, name: &str, cat: InstrumentCategory, market: Market, status: InstrumentStatus) -> MarketInstrument {
        MarketInstrument {
            ts_code: TsCode::parse(ts).unwrap(),
            name: name.to_string(),
            category: cat,
            market,
            board: None,
            sector: None,
            status: Some(status),
            is_st: Some(false),
            publisher: None,
            index_category: None,
            fund_type: None,
            management: None,
            list_date: None,
            source: InstrumentSource::Tushare,
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn upsert_and_get_instrument_roundtrips() {
        let db = make_db();
        let repo = QuotesRepository::new(&db);
        let i = inst("600519.SH", "贵州茅台", InstrumentCategory::Stock, Market::SH, InstrumentStatus::Listed);
        repo.upsert_instruments(&[i.clone()]).unwrap();
        let got = repo.get_instrument(&i.ts_code).unwrap().unwrap();
        assert_eq!(got.name, "贵州茅台");
        assert_eq!(got.category, InstrumentCategory::Stock);
    }

    #[test]
    fn list_universe_ts_codes_skips_delisted() {
        let db = make_db();
        let repo = QuotesRepository::new(&db);
        let listed = inst("600519.SH", "L1", InstrumentCategory::Stock, Market::SH, InstrumentStatus::Listed);
        let delisted = inst("600520.SH", "D1", InstrumentCategory::Stock, Market::SH, InstrumentStatus::Delisted);
        repo.upsert_instruments(&[listed.clone(), delisted.clone()]).unwrap();
        let codes = repo.list_universe_ts_codes().unwrap();
        assert!(codes.iter().any(|c| c.as_str() == "600519.SH"));
        assert!(!codes.iter().any(|c| c.as_str() == "600520.SH"));
    }

    #[test]
    fn list_instruments_total_reflects_filter() {
        let db = make_db();
        let repo = QuotesRepository::new(&db);
        for i in 0..5 {
            let mut x = inst(
                &format!("60000{}.SH", i),
                &format!("S{}", i),
                InstrumentCategory::Stock,
                Market::SH,
                InstrumentStatus::Listed,
            );
            x.name = format!("S{}", i);
            repo.upsert_instruments(&[x]).unwrap();
        }
        let (list, total) = repo
            .list_instruments(Some(InstrumentCategory::Stock), None, 3, 0)
            .unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(total, 5);
    }

    #[test]
    fn list_instruments_query_priority_orders_exact_first() {
        let db = make_db();
        let repo = QuotesRepository::new(&db);
        repo.upsert_instruments(&[
            inst("600519.SH", "AAA", InstrumentCategory::Stock, Market::SH, InstrumentStatus::Listed),
            inst("600520.SH", "AAA Holdings", InstrumentCategory::Stock, Market::SH, InstrumentStatus::Listed),
        ])
        .unwrap();
        let (list, _) = repo.list_instruments(None, Some("AAA"), 10, 0).unwrap();
        assert_eq!(list[0].name, "AAA");
    }

    #[test]
    fn record_and_read_refresh_state_roundtrips() {
        let db = make_db();
        let repo = QuotesRepository::new(&db);
        let td = TradeDate::parse("20260526").unwrap();
        let now = Utc::now();
        repo.record_refresh_state("close", td, 100, 95, 5, now).unwrap();
        let got = repo.read_refresh_state("close", td).unwrap().unwrap();
        assert_eq!(got.0, 100);
        assert_eq!(got.1, 95);
        assert_eq!(got.2, 5);
    }

    #[test]
    fn upsert_daily_basic_idempotent() {
        let db = make_db();
        let repo = QuotesRepository::new(&db);
        let td = TradeDate::parse("20260526").unwrap();
        let row = DailyBasic {
            ts_code: TsCode::parse("600519.SH").unwrap(),
            trade_date: td,
            pe: Some(40.0),
            pe_ttm: Some(38.0),
            pb: None,
            ps: None,
            ps_ttm: None,
            turnover_rate: None,
            turnover_rate_float: None,
            volume_ratio: None,
            total_mv: None,
            circ_mv: None,
            source: "tushare".into(),
            fetched_at: Utc::now(),
        };
        repo.upsert_daily_basic(&[row.clone()]).unwrap();
        repo.upsert_daily_basic(&[row.clone()]).unwrap();
        let got = repo.latest_daily_basic(&row.ts_code, td).unwrap().unwrap();
        assert_eq!(got.pe, Some(40.0));
    }

    #[test]
    fn instrument_category_map_returns_all() {
        let db = make_db();
        let repo = QuotesRepository::new(&db);
        repo.upsert_instruments(&[
            inst("600519.SH", "S", InstrumentCategory::Stock, Market::SH, InstrumentStatus::Listed),
            inst("000001.SH", "Idx", InstrumentCategory::Index, Market::SH, InstrumentStatus::Listed),
        ])
        .unwrap();
        let map = repo.instrument_category_map().unwrap();
        assert_eq!(
            map.get(&TsCode::parse("600519.SH").unwrap()).copied(),
            Some(InstrumentCategory::Stock)
        );
        assert_eq!(
            map.get(&TsCode::parse("000001.SH").unwrap()).copied(),
            Some(InstrumentCategory::Index)
        );
    }

    // ====================================================================== xdxr_events tests

    fn xdxr_div(ts: &str, date: &str, fenhong: f64, songzhuangu: f64) -> XdxrEvent {
        XdxrEvent::dividend_and_split(
            TsCode::parse(ts).unwrap(),
            TradeDate::parse(date).unwrap(),
            Some(fenhong),
            None,
            Some(songzhuangu),
            None,
            1_700_000_000_000,
        )
    }

    #[test]
    fn xdxr_upsert_then_list_roundtrips_fields() {
        let db = make_db();
        let repo = QuotesRepository::new(&db);
        let ts = TsCode::parse("600519.SH").unwrap();
        let e1 = xdxr_div("600519.SH", "20240620", 30.872, 0.0);
        let e2 = xdxr_div("600519.SH", "20230630", 25.911, 0.0);
        let n = repo.upsert_xdxr_events(&ts, &[e1.clone(), e2.clone()]).unwrap();
        assert_eq!(n, 2);
        let got = repo.list_xdxr_events(&ts).unwrap();
        assert_eq!(got.len(), 2);
        // ordered by occur_date ASC
        assert_eq!(got[0].occur_date, e2.occur_date);
        assert_eq!(got[0].fenhong, Some(25.911));
        assert_eq!(got[1].occur_date, e1.occur_date);
        assert_eq!(got[1].category, XdxrCategory::DividendAndSplit);
    }

    #[test]
    fn xdxr_upsert_is_idempotent_on_pk() {
        let db = make_db();
        let repo = QuotesRepository::new(&db);
        let ts = TsCode::parse("600519.SH").unwrap();
        let e = xdxr_div("600519.SH", "20240620", 30.872, 0.0);
        repo.upsert_xdxr_events(&ts, &[e.clone()]).unwrap();
        // 重复写同一 PK，行数仍为 1
        repo.upsert_xdxr_events(&ts, &[e.clone()]).unwrap();
        let got = repo.list_xdxr_events(&ts).unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn xdxr_upsert_updates_existing_row() {
        let db = make_db();
        let repo = QuotesRepository::new(&db);
        let ts = TsCode::parse("600519.SH").unwrap();
        let e1 = xdxr_div("600519.SH", "20240620", 30.872, 0.0);
        let e2 = xdxr_div("600519.SH", "20240620", 31.0, 0.0); // 同 PK，数值变化
        repo.upsert_xdxr_events(&ts, &[e1]).unwrap();
        repo.upsert_xdxr_events(&ts, &[e2]).unwrap();
        let got = repo.list_xdxr_events(&ts).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].fenhong, Some(31.0));
    }

    #[test]
    fn xdxr_delete_clears_only_target_ts_code() {
        let db = make_db();
        let repo = QuotesRepository::new(&db);
        let ts1 = TsCode::parse("600519.SH").unwrap();
        let ts2 = TsCode::parse("000001.SH").unwrap();
        repo.upsert_xdxr_events(&ts1, &[xdxr_div("600519.SH", "20240620", 30.0, 0.0)])
            .unwrap();
        repo.upsert_xdxr_events(&ts2, &[xdxr_div("000001.SH", "20240120", 5.0, 0.0)])
            .unwrap();
        let n = repo.delete_xdxr_events(&ts1).unwrap();
        assert_eq!(n, 1);
        assert!(repo.list_xdxr_events(&ts1).unwrap().is_empty());
        assert_eq!(repo.list_xdxr_events(&ts2).unwrap().len(), 1);
    }

    #[test]
    fn xdxr_supports_different_categories_same_date() {
        let db = make_db();
        let repo = QuotesRepository::new(&db);
        let ts = TsCode::parse("600519.SH").unwrap();
        let date = TradeDate::parse("20240620").unwrap();
        let div = XdxrEvent::dividend_and_split(
            ts.clone(),
            date,
            Some(30.0),
            None,
            None,
            None,
            1_700_000_000_000,
        );
        let equity = XdxrEvent {
            ts_code: ts.clone(),
            occur_date: date,
            category: XdxrCategory::EquityChange,
            fenhong: None,
            peigujia: None,
            songzhuangu: None,
            peigu: None,
            suogu: None,
            xingquanjia: None,
            fenshu: None,
            panqianliutong: Some(1.0e8),
            qianzongguben: Some(1.5e8),
            panhouliutong: Some(1.1e8),
            houzongguben: Some(1.6e8),
            fetched_at: 1_700_000_000_000,
        };
        repo.upsert_xdxr_events(&ts, &[div, equity]).unwrap();
        let got = repo.list_xdxr_events(&ts).unwrap();
        assert_eq!(got.len(), 2);
        let cats: Vec<XdxrCategory> = got.iter().map(|e| e.category).collect();
        assert!(cats.contains(&XdxrCategory::DividendAndSplit));
        assert!(cats.contains(&XdxrCategory::EquityChange));
    }
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<CompanyEvent> {
    let id: String = row.get(0)?;
    let ts_code: String = row.get(1)?;
    let ts_code = TsCode::parse(&ts_code).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let event_type: String = row.get(2)?;
    let announce_date: Option<String> = row.get(3)?;
    let effective_date: Option<String> = row.get(4)?;
    let payload: String = row.get(5)?;
    let source: String = row.get(6)?;
    let fetched_at: String = row.get(7)?;
    Ok(CompanyEvent {
        id,
        ts_code,
        event_type: event_type_from_str(&event_type),
        announce_date: announce_date.and_then(|s| TradeDate::parse(&s).ok()),
        effective_date: effective_date.and_then(|s| TradeDate::parse(&s).ok()),
        payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
        source,
        fetched_at: DateTime::parse_from_rfc3339(&fetched_at)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
    })
}
