//! 交易日历 — Quotes 内部 service。
//!
//! Spec: docs/design/quotes-module.md §3 / §5 (TuShare `trade_cal` 是日历主源)
//!
//! 设计：
//! - 真源是 `quote_trade_calendar` 本地表（由 TuShare `trade_cal` adapter 刷新）。
//! - 表为空 / 当日缺失时回退到"周一到周五"近似（同 `domain::shared::market_time::is_trade_day`）。
//! - shared `resolve_market_time` 目前未接受日历参数（按 §🚨 已反馈，main agent 后续重构）；
//!   Quotes 内部读取路径在调用 `resolve_market_time` 之外，自己再用 `TradeCalendar` 修正
//!   `latestCompletedTradeDate` / `currentTradeDate` 等结论。

use crate::domain::shared::TradeDate;
use crate::infrastructure::db::AppDb;
use chrono::{Datelike, NaiveDate, Weekday};
use rusqlite::OptionalExtension;
use std::collections::HashMap;
use std::sync::RwLock;

/// Spec: quotes-module.md §3
pub trait TradeCalendar: Send + Sync {
    fn is_trade_day(&self, d: NaiveDate) -> bool;

    fn previous_trade_day(&self, d: NaiveDate) -> NaiveDate {
        let mut x = d.pred_opt().expect("date underflow");
        while !self.is_trade_day(x) {
            x = x.pred_opt().expect("date underflow");
        }
        x
    }

    fn next_trade_day(&self, d: NaiveDate) -> NaiveDate {
        let mut x = d.succ_opt().expect("date overflow");
        while !self.is_trade_day(x) {
            x = x.succ_opt().expect("date overflow");
        }
        x
    }
}

/// 默认 fallback：周一到周五。
///
/// ⚠️ 不含 A 股节假日（与 shared::market_time::is_trade_day 一致）。
pub struct WeekdayCalendar;

impl TradeCalendar for WeekdayCalendar {
    fn is_trade_day(&self, d: NaiveDate) -> bool {
        !matches!(d.weekday(), Weekday::Sat | Weekday::Sun)
    }
}

/// 持久化交易日历 repo（`quote_trade_calendar` 表）。
pub struct TradeCalendarRepo {
    db: AppDb,
    cache: RwLock<HashMap<NaiveDate, bool>>,
}

impl TradeCalendarRepo {
    pub fn new(db: AppDb) -> Self {
        Self {
            db,
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// 把 TuShare `trade_cal` 结果批量写入。`(cal_date, is_open, pretrade_date)`。
    pub fn upsert_batch(
        &self,
        rows: &[(TradeDate, bool, Option<TradeDate>)],
        source: &str,
        fetched_at: chrono::DateTime<chrono::Utc>,
    ) -> rusqlite::Result<()> {
        let fetched_str = fetched_at.to_rfc3339();
        self.db.with(|conn| {
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO quote_trade_calendar
                       (cal_date, is_open, pretrade_date, source, fetched_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (cal_date) DO UPDATE SET
                       is_open = excluded.is_open,
                       pretrade_date = excluded.pretrade_date,
                       source = excluded.source,
                       fetched_at = excluded.fetched_at",
                )?;
                for (d, open, pre) in rows {
                    stmt.execute(rusqlite::params![
                        d.format(),
                        if *open { 1 } else { 0 },
                        pre.map(|p| p.format()),
                        source,
                        &fetched_str,
                    ])?;
                }
            }
            tx.commit()?;
            Ok::<(), rusqlite::Error>(())
        })?;
        // invalidate in-memory cache
        self.cache.write().expect("cal cache poisoned").clear();
        Ok(())
    }

    fn query_db(&self, d: NaiveDate) -> Option<bool> {
        let cal = TradeDate::from_naive(d);
        let res: Option<i64> = self
            .db
            .with(|conn| {
                conn.query_row(
                    "SELECT is_open FROM quote_trade_calendar WHERE cal_date = ?1",
                    [cal.format()],
                    |r| r.get::<_, i64>(0),
                )
                .optional()
            })
            .ok()
            .flatten();
        res.map(|n| n != 0)
    }
}

impl TradeCalendar for TradeCalendarRepo {
    fn is_trade_day(&self, d: NaiveDate) -> bool {
        if let Some(v) = self.cache.read().expect("cal cache poisoned").get(&d).copied() {
            return v;
        }
        let v = match self.query_db(d) {
            Some(open) => open,
            None => WeekdayCalendar.is_trade_day(d),
        };
        self.cache.write().expect("cal cache poisoned").insert(d, v);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::db::run_migrations;
    use crate::infrastructure::quotes::migrations as quotes_migrations;
    use chrono::Utc;

    fn make_db() -> AppDb {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|conn| run_migrations(conn, quotes_migrations()).unwrap());
        db
    }

    #[test]
    fn weekday_calendar_recognizes_weekend() {
        let cal = WeekdayCalendar;
        let sat = NaiveDate::from_ymd_opt(2026, 5, 23).unwrap();
        let sun = NaiveDate::from_ymd_opt(2026, 5, 24).unwrap();
        let tue = NaiveDate::from_ymd_opt(2026, 5, 26).unwrap();
        assert!(!cal.is_trade_day(sat));
        assert!(!cal.is_trade_day(sun));
        assert!(cal.is_trade_day(tue));
    }

    #[test]
    fn weekday_calendar_previous_skips_weekend() {
        let cal = WeekdayCalendar;
        let mon = NaiveDate::from_ymd_opt(2026, 5, 25).unwrap();
        // 上一个交易日 = 周五 (5/22)
        let prev = cal.previous_trade_day(mon);
        assert_eq!(prev, NaiveDate::from_ymd_opt(2026, 5, 22).unwrap());
    }

    #[test]
    fn trade_calendar_repo_falls_back_to_weekday_when_db_empty() {
        let db = make_db();
        let repo = TradeCalendarRepo::new(db);
        let sat = NaiveDate::from_ymd_opt(2026, 5, 23).unwrap();
        let tue = NaiveDate::from_ymd_opt(2026, 5, 26).unwrap();
        assert!(!repo.is_trade_day(sat));
        assert!(repo.is_trade_day(tue));
    }

    #[test]
    fn trade_calendar_repo_uses_db_when_present() {
        let db = make_db();
        let repo = TradeCalendarRepo::new(db);
        // 把一个周二标成非交易日（节假日 override）
        let holiday = TradeDate::parse("20260526").unwrap();
        repo.upsert_batch(&[(holiday, false, None)], "tushare", Utc::now())
            .unwrap();
        assert!(!repo.is_trade_day(NaiveDate::from_ymd_opt(2026, 5, 26).unwrap()));
    }
}
