//! A 股交易时段判定——纯函数，无 I/O。
//!
//! 时段：周一至周五 9:30-11:30 + 13:00-15:00（北京时间 UTC+8）。
//! 节假日不识别——盘中时段稍宽不影响业务（节假日 EM 仍返最近收盘价，cache 自然处理）。
//!
//! 调用方：scheduler（决定刷新间隔）/ cache 层（决定 TTL）。

use chrono::{Datelike, Timelike, Weekday};

/// 当前是否处于 A 股盘中时段。
/// 用 UTC + 8 偏移得北京时间，不依赖系统时区配置。
pub fn is_a_share_trading_hours() -> bool {
    let beijing = chrono::Utc::now() + chrono::Duration::hours(8);
    if matches!(beijing.weekday(), Weekday::Sat | Weekday::Sun) {
        return false;
    }
    let minute_of_day = beijing.hour() * 60 + beijing.minute();
    // 早盘 9:30-11:30 → 570-690；午盘 13:00-15:00 → 780-900
    (570..=690).contains(&minute_of_day) || (780..=900).contains(&minute_of_day)
}
