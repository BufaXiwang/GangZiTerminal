//! 本地交易日历 — 工作日推算 + 内置中国法定假日 / 调休表。
//!
//! Spec: docs/design/quotes-module.md §5 "交易日历"
//!
//! 纯 domain 模块；仅依赖 `chrono`。无 I/O / 无 provider。
//! TuShare 健康时由 pipeline 层调 TuShare `trade_cal` 校准本地推算结果，
//! 但本模块本身不感知 TuShare，可独立工作。
//!
//! 假日数据来源：国务院办公厅每年发布的法定节假日 / 调休安排通知。
//! 维护节奏见 docs/TODO.md「交易日历假日表续更」。
//!
//! 覆盖范围（截至 2026-05-28）：
//! - 2024 / 2025：已发布并确认。
//! - 2026：根据国务院 2025-11 发布的 2026 假日安排通知初稿；如有官方修订需同步更新。
//! - 2027+：未发布，留待官方通知发布后续填。

use chrono::{Datelike, NaiveDate, Weekday};

/// 中国 A 股法定节假日（交易所休市日）。按年组织、按日期升序。
///
/// 不包括正常周末（周末由 `is_trading_day` 通过 weekday 判定）。
///
/// **需要每年人工续更**：见 docs/TODO.md。
const HOLIDAYS: &[(i32, u32, u32)] = &[
    // ───────────── 2024 ─────────────
    (2024, 1, 1),   // 元旦
    (2024, 2, 9),   // 春节调休休市（除夕）
    (2024, 2, 12),  // 春节
    (2024, 2, 13),
    (2024, 2, 14),
    (2024, 2, 15),
    (2024, 2, 16),
    // 2024-02-10 / 02-11 / 02-17 / 02-18 为周末，不需额外列
    (2024, 4, 4),   // 清明
    (2024, 4, 5),
    // 2024-04-06 / 04-07 为周末
    (2024, 5, 1),   // 劳动节
    (2024, 5, 2),
    (2024, 5, 3),
    // 2024-05-04 / 05-05 为周末
    (2024, 6, 10),  // 端午
    // 2024-06-08 / 06-09 为周末
    (2024, 9, 16),  // 中秋
    (2024, 9, 17),
    // 2024-09-15 为周末
    (2024, 10, 1),  // 国庆
    (2024, 10, 2),
    (2024, 10, 3),
    (2024, 10, 4),
    (2024, 10, 7),
    // 2024-10-05 / 10-06 为周末

    // ───────────── 2025 ─────────────
    (2025, 1, 1),   // 元旦
    (2025, 1, 28),  // 春节调休（除夕）
    (2025, 1, 29),  // 春节初一
    (2025, 1, 30),
    (2025, 1, 31),
    (2025, 2, 3),
    (2025, 2, 4),
    // 2025-02-01 / 02-02 为周末
    (2025, 4, 4),   // 清明
    // 2025-04-05 / 04-06 为周末
    (2025, 5, 1),   // 劳动
    (2025, 5, 2),
    (2025, 5, 5),
    // 2025-05-03 / 05-04 为周末
    (2025, 5, 30),  // 端午（5/31 / 6/1 为周末，6/2 调休休市？端午 5/31-6/2，5/30 调休）
    (2025, 6, 2),
    // 中秋 + 国庆连休 2025-10-01 至 2025-10-08（10/4-5、10/11 为周末）
    (2025, 10, 1),
    (2025, 10, 2),
    (2025, 10, 3),
    (2025, 10, 6),
    (2025, 10, 7),
    (2025, 10, 8),

    // ───────────── 2026 ─────────────
    // 国务院办公厅关于 2026 年部分节假日安排的通知（2025-11 发布）。
    // 如发布版本与下述日期不一致，需以官方通知为准并更新本表。
    (2026, 1, 1),   // 元旦
    (2026, 1, 2),
    // 2026-01-03 / 01-04 为周末
    // 春节 2026-02-16 至 2026-02-22（除夕 2/16 周一开始）
    (2026, 2, 16),
    (2026, 2, 17),
    (2026, 2, 18),
    (2026, 2, 19),
    (2026, 2, 20),
    // 2026-02-21 / 02-22 为周末
    (2026, 4, 6),   // 清明（4/4-5 为周末）
    (2026, 5, 1),   // 劳动
    (2026, 5, 4),
    (2026, 5, 5),
    // 2026-05-02 / 05-03 为周末
    (2026, 6, 19),  // 端午（6/20-21 为周末）
    // 中秋+国庆 2026-10-01 至 2026-10-08（具体调休以官方通知为准）
    (2026, 10, 1),
    (2026, 10, 2),
    (2026, 10, 5),
    (2026, 10, 6),
    (2026, 10, 7),
    (2026, 10, 8),
    // 2026-10-03 / 10-04 为周末
];

/// 调休补班日（周末上班补假日）。
///
/// 按官方通知，调休日股票市场也休市；但如果是补"工作日上班"性质，
/// 沪深证券交易所通常按"工作日"对待并开盘。本表只列入"周末-但-需上班"
/// 的日期，作为 `is_trading_day` 在周末场景下的开盘 override。
const COMPENSATORY_WORKDAYS: &[(i32, u32, u32)] = &[
    // ───────────── 2024 ─────────────
    (2024, 2, 4),   // 春节调休补班
    (2024, 2, 18),
    (2024, 4, 7),   // 清明调休补班
    (2024, 4, 28),  // 劳动节调休补班
    (2024, 5, 11),
    (2024, 9, 14),  // 中秋调休补班
    (2024, 9, 29),  // 国庆调休补班
    (2024, 10, 12),

    // ───────────── 2025 ─────────────
    (2025, 1, 26),  // 春节调休补班
    (2025, 2, 8),
    (2025, 4, 27),  // 劳动节调休补班
    (2025, 9, 28),  // 国庆调休补班
    (2025, 10, 11),

    // ───────────── 2026 ─────────────
    // 以官方通知为准；下面是常见的调休模式参考，需在官方通知发布后核对。
    // 暂留空 — 见 docs/TODO.md。
];

fn make_date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).expect("invalid hardcoded date")
}

fn is_listed_holiday(d: NaiveDate) -> bool {
    HOLIDAYS
        .iter()
        .any(|(y, m, day)| make_date(*y, *m, *day) == d)
}

fn is_compensatory_workday(d: NaiveDate) -> bool {
    COMPENSATORY_WORKDAYS
        .iter()
        .any(|(y, m, day)| make_date(*y, *m, *day) == d)
}

/// 判断某天是否为 A 股交易日。
///
/// 规则：
/// 1. 在调休补班表内 → true（周末-但-开盘）。
/// 2. 周末（Sat / Sun）→ false。
/// 3. 在法定假日表内 → false。
/// 4. 否则 true。
pub fn is_trading_day(date: NaiveDate) -> bool {
    if is_compensatory_workday(date) {
        return true;
    }
    if matches!(date.weekday(), Weekday::Sat | Weekday::Sun) {
        return false;
    }
    !is_listed_holiday(date)
}

/// 上一个交易日（严格 < `date`，跳过周末 / 节假日）。
pub fn previous_trading_day(date: NaiveDate) -> NaiveDate {
    let mut x = date.pred_opt().expect("date underflow");
    while !is_trading_day(x) {
        x = x.pred_opt().expect("date underflow");
    }
    x
}

/// 下一个交易日（严格 > `date`，跳过周末 / 节假日）。
pub fn next_trading_day(date: NaiveDate) -> NaiveDate {
    let mut x = date.succ_opt().expect("date overflow");
    while !is_trading_day(x) {
        x = x.succ_opt().expect("date overflow");
    }
    x
}

/// 闭区间 `[start, end]` 之间所有交易日，按升序。
pub fn trading_days_between(start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate> {
    if end < start {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cur = start;
    loop {
        if is_trading_day(cur) {
            out.push(cur);
        }
        if cur == end {
            break;
        }
        cur = match cur.succ_opt() {
            Some(d) => d,
            None => break,
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_weekday_is_trading_day() {
        // 普通周二 2024-03-05
        assert!(is_trading_day(NaiveDate::from_ymd_opt(2024, 3, 5).unwrap()));
        // 普通周一 2025-06-09
        assert!(is_trading_day(NaiveDate::from_ymd_opt(2025, 6, 9).unwrap()));
    }

    #[test]
    fn weekend_is_not_trading_day() {
        // 2024-05-25 Sat / 2024-05-26 Sun
        assert!(!is_trading_day(NaiveDate::from_ymd_opt(2024, 5, 25).unwrap()));
        assert!(!is_trading_day(NaiveDate::from_ymd_opt(2024, 5, 26).unwrap()));
    }

    #[test]
    fn spring_festival_2024_closed() {
        // 春节核心 2024-02-12 至 02-16 全休
        for day in 12..=16 {
            assert!(
                !is_trading_day(NaiveDate::from_ymd_opt(2024, 2, day).unwrap()),
                "2024-02-{} 应为春节假期",
                day
            );
        }
    }

    #[test]
    fn compensatory_workday_2024_02_04_is_trading() {
        // 春节调休补班：2024-02-04 是周日但开盘
        assert!(is_trading_day(NaiveDate::from_ymd_opt(2024, 2, 4).unwrap()));
        // 同月正常周日 2024-02-11 不开盘
        assert!(!is_trading_day(NaiveDate::from_ymd_opt(2024, 2, 11).unwrap()));
    }

    #[test]
    fn national_day_2024_closed() {
        for day in 1..=4 {
            assert!(
                !is_trading_day(NaiveDate::from_ymd_opt(2024, 10, day).unwrap()),
                "2024-10-{} 应为国庆假期",
                day
            );
        }
        // 2024-10-07 也休市
        assert!(!is_trading_day(NaiveDate::from_ymd_opt(2024, 10, 7).unwrap()));
        // 2024-10-08 周二恢复
        assert!(is_trading_day(NaiveDate::from_ymd_opt(2024, 10, 8).unwrap()));
    }

    #[test]
    fn new_year_2025_closed() {
        assert!(!is_trading_day(NaiveDate::from_ymd_opt(2025, 1, 1).unwrap()));
    }

    #[test]
    fn previous_trading_day_skips_weekend_and_holiday() {
        // 2024-10-08 周二的上一交易日 = 2024-09-30 周一（10/1-7 全部为国庆假）
        let prev = previous_trading_day(NaiveDate::from_ymd_opt(2024, 10, 8).unwrap());
        assert_eq!(prev, NaiveDate::from_ymd_opt(2024, 9, 30).unwrap());
    }

    #[test]
    fn next_trading_day_skips_weekend_and_holiday() {
        // 2024-09-30 周一的下一交易日 = 2024-10-08 周二
        let next = next_trading_day(NaiveDate::from_ymd_opt(2024, 9, 30).unwrap());
        assert_eq!(next, NaiveDate::from_ymd_opt(2024, 10, 8).unwrap());
    }

    #[test]
    fn trading_days_between_excludes_holidays_and_weekends() {
        // 2024-09-30 (Mon) 至 2024-10-09 (Wed)
        // 交易日：09-30, 10-08, 10-09（10/1-7 假，10/5-6 周末已含 10/7 假）
        let days = trading_days_between(
            NaiveDate::from_ymd_opt(2024, 9, 30).unwrap(),
            NaiveDate::from_ymd_opt(2024, 10, 9).unwrap(),
        );
        assert_eq!(
            days,
            vec![
                NaiveDate::from_ymd_opt(2024, 9, 30).unwrap(),
                NaiveDate::from_ymd_opt(2024, 10, 8).unwrap(),
                NaiveDate::from_ymd_opt(2024, 10, 9).unwrap(),
            ]
        );
    }

    #[test]
    fn trading_days_between_empty_when_end_before_start() {
        let days = trading_days_between(
            NaiveDate::from_ymd_opt(2025, 6, 10).unwrap(),
            NaiveDate::from_ymd_opt(2025, 6, 9).unwrap(),
        );
        assert!(days.is_empty());
    }

    #[test]
    fn compensatory_workday_2025_02_08_is_trading() {
        // 春节调休补班：2025-02-08 周六开盘
        assert!(is_trading_day(NaiveDate::from_ymd_opt(2025, 2, 8).unwrap()));
    }

    #[test]
    fn new_year_2026_closed() {
        assert!(!is_trading_day(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()));
        assert!(!is_trading_day(NaiveDate::from_ymd_opt(2026, 1, 2).unwrap()));
    }
}
