//! 时间 newtype——`TradeDate`（YYYYMMDD 整数）/ `OccurredAt`（unix ms）。
//!
//! - `TradeDate` 用整数表示，可比较、可排序，不和"展示用日期串"混
//! - `OccurredAt` 是真源时间，避免在不同时区 parse 字符串

use serde::{Deserialize, Serialize};

/// 交易日——YYYYMMDD 紧凑整数（如 20260513）。
///
/// 不是任意日期：构造时只校验"四位年 + 两位月（01-12）+ 两位日（01-31）"格式，
/// 不知道是否真是交易日（要查 `TradeCalendar`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TradeDate(i32);

impl TradeDate {
    /// 从 YYYYMMDD 整数构造。
    pub fn new(yyyymmdd: i32) -> Result<Self, TimeError> {
        let y = yyyymmdd / 10000;
        let m = (yyyymmdd / 100) % 100;
        let d = yyyymmdd % 100;
        if !(1900..=2100).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return Err(TimeError::BadDate(yyyymmdd));
        }
        Ok(Self(yyyymmdd))
    }

    pub fn from_unchecked(yyyymmdd: i32) -> Self {
        Self(yyyymmdd)
    }

    /// 从 "YYYY-MM-DD" 字符串构造。
    pub fn from_iso(s: &str) -> Result<Self, TimeError> {
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() != 3 {
            return Err(TimeError::BadDateStr(s.into()));
        }
        let y: i32 = parts[0]
            .parse()
            .map_err(|_| TimeError::BadDateStr(s.into()))?;
        let m: i32 = parts[1]
            .parse()
            .map_err(|_| TimeError::BadDateStr(s.into()))?;
        let d: i32 = parts[2]
            .parse()
            .map_err(|_| TimeError::BadDateStr(s.into()))?;
        Self::new(y * 10000 + m * 100 + d)
    }

    /// 从 "YYYYMMDD" 紧凑字符串构造。
    pub fn from_compact(s: &str) -> Result<Self, TimeError> {
        let n: i32 = s.parse().map_err(|_| TimeError::BadDateStr(s.into()))?;
        Self::new(n)
    }

    /// 转 "YYYY-MM-DD"。
    pub fn to_iso(&self) -> String {
        let y = self.0 / 10000;
        let m = (self.0 / 100) % 100;
        let d = self.0 % 100;
        format!("{:04}-{:02}-{:02}", y, m, d)
    }

    /// 转 "YYYYMMDD"（TuShare 接口入参用）。
    pub fn to_compact(&self) -> String {
        format!("{:08}", self.0)
    }

    pub fn value(&self) -> i32 {
        self.0
    }

    /// 加 N 个日历日（不考虑节假日）。需要"下个交易日"用 `TradeCalendar::next_trading_day`。
    pub fn add_calendar_days(&self, days: i64) -> Self {
        let chrono_date = self.to_chrono();
        let next = chrono_date + chrono::Duration::days(days);
        Self::new(next.year() * 10000 + next.month() as i32 * 100 + next.day() as i32)
            .expect("add_calendar_days produced valid date")
    }

    fn to_chrono(&self) -> chrono::NaiveDate {
        let y = self.0 / 10000;
        let m = (self.0 / 100) % 100;
        let d = self.0 % 100;
        chrono::NaiveDate::from_ymd_opt(y, m as u32, d as u32)
            .expect("TradeDate invariant: valid Y/M/D")
    }

    /// 北京时间今日。
    pub fn today_beijing() -> Self {
        let beijing = chrono::Utc::now() + chrono::Duration::hours(8);
        let d = beijing.date_naive();
        Self::new(d.year() * 10000 + d.month() as i32 * 100 + d.day() as i32)
            .expect("today is valid date")
    }
}

use chrono::Datelike;

impl std::fmt::Display for TradeDate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_iso())
    }
}

/// 时间戳 unix ms——内部时间真源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OccurredAt(i64);

impl OccurredAt {
    pub fn new(ms: i64) -> Self {
        Self(ms)
    }

    pub fn now() -> Self {
        Self(chrono::Utc::now().timestamp_millis())
    }

    pub fn value(&self) -> i64 {
        self.0
    }

    /// 转 RFC3339 字符串（仅用于展示 / 日志）。
    pub fn to_rfc3339(&self) -> String {
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(self.0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| format!("invalid:{}", self.0))
    }

    /// 距离 now 多久（秒）。
    pub fn age_secs(&self) -> i64 {
        (Self::now().0 - self.0) / 1000
    }
}

impl std::fmt::Display for OccurredAt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_rfc3339())
    }
}

// ===== 错误 ===============================================================

#[derive(Debug, Clone, thiserror::Error)]
pub enum TimeError {
    #[error("非法日期 yyyymmdd：{0}")]
    BadDate(i32),
    #[error("非法日期串：{0}")]
    BadDateStr(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trade_date_construction() {
        assert!(TradeDate::new(20260513).is_ok());
        assert!(TradeDate::new(20261301).is_err()); // bad month
        assert!(TradeDate::new(20260200).is_err()); // bad day
    }

    #[test]
    fn trade_date_iso_round_trip() {
        let td = TradeDate::from_iso("2026-05-13").unwrap();
        assert_eq!(td.value(), 20260513);
        assert_eq!(td.to_iso(), "2026-05-13");
        assert_eq!(td.to_compact(), "20260513");
    }

    #[test]
    fn trade_date_compact_parse() {
        let td = TradeDate::from_compact("20260513").unwrap();
        assert_eq!(td.value(), 20260513);
    }

    #[test]
    fn trade_date_ordering() {
        let a = TradeDate::new(20260512).unwrap();
        let b = TradeDate::new(20260513).unwrap();
        assert!(a < b);
    }

    #[test]
    fn trade_date_add_days() {
        let td = TradeDate::new(20260513).unwrap();
        assert_eq!(td.add_calendar_days(1).value(), 20260514);
        assert_eq!(td.add_calendar_days(-1).value(), 20260512);
        // 跨月
        let end_of_april = TradeDate::new(20260430).unwrap();
        assert_eq!(end_of_april.add_calendar_days(1).value(), 20260501);
    }

    #[test]
    fn occurred_at_age() {
        let now = OccurredAt::now();
        // age 应该接近 0
        assert!(now.age_secs().abs() < 2);
    }
}
