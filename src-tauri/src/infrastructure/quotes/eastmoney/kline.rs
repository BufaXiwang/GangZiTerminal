//! EM push2his K 线 + 分钟 K 接入。
//!
//! - `fetch_minute_klines`：分钟 K（1m / 5m / 15m / 30m / 60m）
//!   端点：push2his.eastmoney.com/api/qt/stock/kline/get?klt=N
//!   klt 编码：1=1m / 5=5m / 15=15m / 30=30m / 60=60m / 101=day / 102=week / 103=month
//!   优点：免费 / 无积分门槛（vs TuShare stk_mins 需要 5000+）
//!
//! - `fetch_minutes_intraday`：当日分时图（trends2 端点）

use super::client::{fetch_text_with_retry, parse_em_response};
use crate::domain::quotes::{
    HistorySource, MinuteKlinePoint, MinuteKlineSeries, MinutePeriod, MinutePoint, QuotesError,
};
use crate::domain::shared::{Lots, OccurredAt, StockCode, TradeDate, TsCode, Yuan};

// ============================================================================
// 分钟 K（1/5/15/30/60m）
// ============================================================================

pub async fn fetch_minute_klines(
    ts_code: &TsCode,
    period: MinutePeriod,
    limit: usize,
) -> Result<MinuteKlineSeries, QuotesError> {
    let klt = match period {
        MinutePeriod::M1 => 1,
        MinutePeriod::M5 => 5,
        MinutePeriod::M15 => 15,
        MinutePeriod::M30 => 30,
        MinutePeriod::M60 => 60,
    };
    let limit = limit.clamp(30, 800);
    let secid = ts_code.to_em_secid();
    let url = format!(
        "https://push2his.eastmoney.com/api/qt/stock/kline/get?\
         secid={secid}&fields1=f1,f2,f3,f4,f5,f6&fields2=f51,f52,f53,f54,f55,f56,f57\
         &klt={klt}&fqt=1&end=20500101&lmt={limit}"
    );
    let body = fetch_text_with_retry(&url, "分钟K").await?;
    let value = parse_em_response(&body, "分钟K")?;

    let klines = value
        .pointer("/data/klines")
        .and_then(|v| v.as_array())
        .ok_or_else(|| QuotesError::Decode("klines 响应缺 data.klines".into()))?;

    // 每行格式："YYYY-MM-DD HH:MM,open,close,high,low,volume(手),amount(元),..."
    let mut points: Vec<MinuteKlinePoint> = klines
        .iter()
        .filter_map(|line| line.as_str())
        .filter_map(parse_minute_kline_line)
        .collect();

    // 一定升序——但保险起见排一下
    points.sort_by_key(|p| p.timestamp.value());

    let date = points
        .last()
        .and_then(|p| extract_trade_date(p.timestamp))
        .unwrap_or_else(TradeDate::today_beijing);

    let code = StockCode::new(ts_code.code())?;
    Ok(MinuteKlineSeries {
        code,
        period,
        date,
        points,
        source: HistorySource::Eastmoney,
        stale: false,
    })
}

fn parse_minute_kline_line(line: &str) -> Option<MinuteKlinePoint> {
    let fields: Vec<&str> = line.split(',').collect();
    if fields.len() < 7 {
        return None;
    }
    let time_str = fields[0]; // "YYYY-MM-DD HH:MM" 或 "YYYY-MM-DD"（日 K）
    let ts = parse_dt_to_ms(time_str)?;
    Some(MinuteKlinePoint {
        timestamp: OccurredAt::new(ts),
        open: Yuan::from_unchecked(fields[1].parse().ok()?),
        close: Yuan::from_unchecked(fields[2].parse().ok()?),
        high: Yuan::from_unchecked(fields[3].parse().ok()?),
        low: Yuan::from_unchecked(fields[4].parse().ok()?),
        volume: Lots::from_unchecked(fields[5].parse().ok()?),
        amount: Yuan::from_unchecked(fields[6].parse().ok()?),
        vwap: None, // 由 caller / 客户端按需算
    })
}

fn parse_dt_to_ms(s: &str) -> Option<i64> {
    // 容错："YYYY-MM-DD HH:MM" 或 "YYYY-MM-DD"
    let parts: Vec<&str> = s.split_whitespace().collect();
    let date_str = parts.first()?;
    let date_parts: Vec<&str> = date_str.split('-').collect();
    if date_parts.len() != 3 {
        return None;
    }
    let y: i32 = date_parts[0].parse().ok()?;
    let m: u32 = date_parts[1].parse().ok()?;
    let d: u32 = date_parts[2].parse().ok()?;
    let (hh, mm) = if let Some(time_str) = parts.get(1) {
        let tparts: Vec<&str> = time_str.split(':').collect();
        let h: u32 = tparts.first()?.parse().ok()?;
        let m: u32 = tparts.get(1)?.parse().ok()?;
        (h, m)
    } else {
        (15, 0) // 日 K 默认收盘时刻
    };
    // 北京时间 → UTC（减 8 小时）
    let utc = chrono::NaiveDate::from_ymd_opt(y, m, d)?
        .and_hms_opt(hh.saturating_sub(8), mm, 0)?
        .and_utc();
    Some(utc.timestamp_millis())
}

fn extract_trade_date(ts: OccurredAt) -> Option<TradeDate> {
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ts.value())?;
    let beijing = dt + chrono::Duration::hours(8);
    let date = beijing.date_naive();
    TradeDate::new(date.year() * 10000 + date.month() as i32 * 100 + date.day() as i32).ok()
}

use chrono::Datelike;

// ============================================================================
// 当日分时图（push2his trends2）
// ============================================================================

pub async fn fetch_minutes_intraday(
    ts_code: &TsCode,
    days: usize,
) -> Result<Vec<MinutePoint>, QuotesError> {
    let secid = ts_code.to_em_secid();
    let days = days.clamp(1, 5);
    let url = format!(
        "https://push2his.eastmoney.com/api/qt/stock/trends2/get?\
         secid={secid}&fields1=f1,f2,f3,f4,f5,f6,f7,f8,f9,f10,f11\
         &fields2=f51,f52,f53,f54,f55,f56,f57,f58&iscr=0&iscca=0&ndays={days}"
    );
    let body = fetch_text_with_retry(&url, "分时").await?;
    let value = parse_em_response(&body, "分时")?;
    let trends = value
        .pointer("/data/trends")
        .and_then(|v| v.as_array())
        .ok_or_else(|| QuotesError::Decode("trends2 响应缺 data.trends".into()))?;

    let points: Vec<MinutePoint> = trends
        .iter()
        .filter_map(|v| v.as_str())
        .filter_map(parse_minute_line)
        .collect();
    Ok(filter_latest_session(points))
}

fn parse_minute_line(line: &str) -> Option<MinutePoint> {
    let fields: Vec<&str> = line.split(',').collect();
    if fields.len() < 3 {
        return None;
    }
    Some(MinutePoint {
        time: fields[0].to_string(),
        price: Yuan::from_unchecked(fields.get(2)?.parse().ok()?),
        average: fields
            .get(7)
            .and_then(|s| s.parse::<f64>().ok())
            .map(Yuan::from_unchecked),
        volume: fields
            .get(5)
            .and_then(|s| s.parse::<i64>().ok())
            .map(Lots::from_unchecked),
        amount: fields
            .get(6)
            .and_then(|s| s.parse::<f64>().ok())
            .map(Yuan::from_unchecked),
    })
}

fn filter_latest_session(points: Vec<MinutePoint>) -> Vec<MinutePoint> {
    let latest_date = points
        .iter()
        .filter_map(|p| p.time.get(0..10))
        .max()
        .map(str::to_string);
    match latest_date {
        Some(d) => points
            .into_iter()
            .filter(|p| p.time.starts_with(&d))
            .collect(),
        None => points,
    }
}
