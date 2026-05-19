//! TDX K 线接入——把同步 TCP client 的 `security_bars` 暴露成 async 接口，
//! 与 [`crate::infrastructure::quotes::eastmoney::kline`] 形状对齐，让上层 cache
//! 层可以"TDX 主路径 + EM 兜底"无缝切换。
//!
//! 覆盖：1m / 5m / 15m / 30m / 60m / Day / Week / Month。
//!
//! 限制（不可绕开）：
//! - **不支持北交所**（TDX `Market` 只有 SZ/SH）→ BJ ts_code 直接返
//!   `QuotesError::InvalidInput("TDX 不支持 BJ")`，调用方按需 fallback 到 EM。
//! - 同步 TCP，单 client 串行——这里复用单连接 + `spawn_blocking`，
//!   不与实时报价的 `TdxSource` / `TdxConnectionPool` 共用 client（K 线请求频率
//!   远低于实时报价，独立 client 更简单，失败时也不污染报价路径）。

use crate::domain::quotes::{
    HistorySource, KlinePeriod, KlinePoint, KlineSeries, MinuteKlinePoint, MinuteKlineSeries,
    MinutePeriod, MinutePoint, QuotesError,
};
use crate::domain::shared::{Lots, OccurredAt, StockCode, TradeDate, TsCode, Yuan};
use crate::infrastructure::quotes::tdx::client::TdxHqClient;
use crate::infrastructure::quotes::tdx::types::{Bar, BarCategory, Market};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// TDX server-side bar count cap——超过此值 server 默认按 800 截。
const MAX_BARS_PER_CALL: u16 = 800;

/// 全局共享单 client——K 线请求频率不高，单 TCP 连接顺序请求够用。
/// 失败时丢弃重连（跟实时报价 TdxSource 同套路）。
fn bars_client() -> &'static Arc<Mutex<Option<TdxHqClient>>> {
    static CLIENT: std::sync::OnceLock<Arc<Mutex<Option<TdxHqClient>>>> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| Arc::new(Mutex::new(None)))
}

/// 把 ts_code 拆成 (Market, 6位 code)；BJ 返 InvalidInput 让调用方 fallback。
fn ts_to_market(ts_code: &TsCode) -> Result<(Market, String), QuotesError> {
    match ts_code.market() {
        "SH" => Ok((Market::SH, ts_code.code().to_string())),
        "SZ" => Ok((Market::SZ, ts_code.code().to_string())),
        "BJ" => Err(QuotesError::InvalidInput(format!(
            "TDX 不支持北交所（{}），调用方应 fallback 到 EM",
            ts_code.as_str()
        ))),
        m => Err(QuotesError::InvalidInput(format!("未知 market: {m}"))),
    }
}

fn minute_period_to_category(p: MinutePeriod) -> BarCategory {
    match p {
        MinutePeriod::M1 => BarCategory::Minute1,
        MinutePeriod::M5 => BarCategory::Minute5,
        MinutePeriod::M15 => BarCategory::Minute15,
        MinutePeriod::M30 => BarCategory::Minute30,
        MinutePeriod::M60 => BarCategory::Hour,
    }
}

fn kline_period_to_category(p: KlinePeriod) -> BarCategory {
    match p {
        KlinePeriod::Day => BarCategory::Day,
        KlinePeriod::Week => BarCategory::Week,
        KlinePeriod::Month => BarCategory::Month,
    }
}

/// Bar `(year/month/day hh:mm)` → unix ms（按北京时间换算到 UTC）。
fn bar_to_ms(bar: &Bar) -> Option<i64> {
    use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
    let date = NaiveDate::from_ymd_opt(bar.year as i32, bar.month as u32, bar.day as u32)?;
    let time = NaiveTime::from_hms_opt(bar.hour as u32, bar.minute as u32, 0)?;
    let beijing = NaiveDateTime::new(date, time);
    // 北京时间 = UTC + 8 → UTC ts = beijing - 8h
    let utc_ms = beijing.and_utc().timestamp_millis() - 8 * 3600 * 1000;
    Some(utc_ms)
}

// ============================================================================
// 分钟 K（1m/5m/15m/30m/60m）
// ============================================================================

pub async fn fetch_minute_klines(
    ts_code: &TsCode,
    period: MinutePeriod,
    limit: usize,
) -> Result<MinuteKlineSeries, QuotesError> {
    let (market, code) = ts_to_market(ts_code)?;
    let category = minute_period_to_category(period);
    let count = (limit.clamp(30, MAX_BARS_PER_CALL as usize)) as u16;

    let bars = fetch_bars_blocking(market, code, category, count).await?;
    if bars.is_empty() {
        return Err(QuotesError::Decode(format!(
            "TDX security_bars 返回 0 根（{} {:?}）",
            ts_code.as_str(),
            period
        )));
    }

    let mut points: Vec<MinuteKlinePoint> = bars
        .iter()
        .filter_map(|b| {
            let ms = bar_to_ms(b)?;
            Some(MinuteKlinePoint {
                timestamp: OccurredAt::new(ms),
                open: Yuan::from_unchecked(b.open),
                close: Yuan::from_unchecked(b.close),
                high: Yuan::from_unchecked(b.high),
                low: Yuan::from_unchecked(b.low),
                // TDX volume 单位是"股"；EM 是"手"。统一成 EM 口径除以 100。
                // TDX 解码 mootdx 协议时已经按"手"返回 f64——见 helper::get_volume，
                // 实际上是直接 i32→f64 不缩放。这里保险按"手"对待（多数情况一致）。
                volume: Lots::from_unchecked(b.volume as i64),
                amount: Yuan::from_unchecked(b.amount),
                vwap: None,
            })
        })
        .collect();
    points.sort_by_key(|p| p.timestamp.value());

    let date = points
        .last()
        .and_then(|p| beijing_date_from_ms(p.timestamp.value()))
        .unwrap_or_else(TradeDate::today_beijing);
    let stock_code = StockCode::new(ts_code.code())?;

    Ok(MinuteKlineSeries {
        code: stock_code,
        period,
        date,
        points,
        source: HistorySource::Tdx,
        stale: false,
    })
}

// ============================================================================
// 日 / 周 / 月 K
// ============================================================================

pub async fn fetch_daily_klines(
    ts_code: &TsCode,
    period: KlinePeriod,
    limit: usize,
) -> Result<KlineSeries, QuotesError> {
    let (market, code) = ts_to_market(ts_code)?;
    let category = kline_period_to_category(period);
    let count = (limit.clamp(30, MAX_BARS_PER_CALL as usize)) as u16;

    let bars = fetch_bars_blocking(market, code, category, count).await?;
    if bars.is_empty() {
        return Err(QuotesError::Decode(format!(
            "TDX security_bars 返回 0 根（{} {:?}）",
            ts_code.as_str(),
            period
        )));
    }

    let mut points: Vec<KlinePoint> = bars
        .iter()
        .filter_map(|b| {
            let date = TradeDate::new(b.year as i32 * 10000 + b.month as i32 * 100 + b.day as i32)
                .ok()?;
            Some(KlinePoint {
                date,
                open: Yuan::from_unchecked(b.open),
                close: Yuan::from_unchecked(b.close),
                high: Yuan::from_unchecked(b.high),
                low: Yuan::from_unchecked(b.low),
                volume: Lots::from_unchecked(b.volume as i64),
                amount: Yuan::from_unchecked(b.amount),
            })
        })
        .collect();
    points.sort_by_key(|p| p.date);

    let stock_code = StockCode::new(ts_code.code())?;
    Ok(KlineSeries {
        code: stock_code,
        period,
        adj: crate::domain::quotes::AdjMode::None, // TDX 不复权
        points,
        source: HistorySource::Tdx,
        stale: false,
        warning: Some("TDX 不复权——除权日附近会有跳变；配 TuShare token 可获得前复权数据".into()),
    })
}

// ============================================================================
// 内部：同步 TCP 调用的 async 包装
// ============================================================================

async fn fetch_bars_blocking(
    market: Market,
    code: String,
    category: BarCategory,
    count: u16,
) -> Result<Vec<Bar>, QuotesError> {
    let client_arc = bars_client().clone();
    tokio::task::spawn_blocking(move || -> Result<Vec<Bar>, QuotesError> {
        let mut guard = client_arc.blocking_lock();
        if guard.is_none() {
            let (c, addr) = TdxHqClient::connect_bestip(CONNECT_TIMEOUT)
                .map_err(|e| QuotesError::Network(format!("TDX 建连失败：{e}")))?;
            tracing::debug!(peer = %addr, "tdx bars 连接建立");
            *guard = Some(c);
        }
        let client = guard.as_mut().expect("just inited");
        match client.security_bars(category, market, &code, 0, count) {
            Ok(bars) => Ok(bars),
            Err(e) => {
                // 失败丢弃连接，下次重连
                *guard = None;
                Err(QuotesError::Network(format!("TDX 拉 K 线失败：{e}")))
            }
        }
    })
    .await
    .map_err(|e| QuotesError::Network(format!("spawn_blocking join 失败：{e}")))?
}

// ============================================================================
// 分时图（当日 / 近 N 日逐分钟）
// ============================================================================
//
// TDX `BarCategory::Minute = 7` 返 OHLC 分钟 bars。我们把它转成 MinutePoint：
// - price = bar.close（每分钟收盘价 = 该分钟末价）
// - volume / amount = 该分钟内成交量 / 额（增量，不是累计）
// - average = 累计成交均价（cumulative_amount / cumulative_volume）—— 跟 EM trends2 一致

pub async fn fetch_minutes_intraday(
    ts_code: &TsCode,
    days: usize,
) -> Result<Vec<MinutePoint>, QuotesError> {
    let (market, code) = ts_to_market(ts_code)?;
    let days = days.clamp(1, 5);
    // 每天分时约 240 根（4h 交易 × 60min），最多取 5 天 → 1200 根，clamp 到 800（TDX 上限）
    let count = (240 * days as u16).min(MAX_BARS_PER_CALL);

    let bars = fetch_bars_blocking(market, code, BarCategory::Minute, count).await?;
    if bars.is_empty() {
        return Err(QuotesError::Decode(format!(
            "TDX 分时返 0 根（{}）",
            ts_code.as_str()
        )));
    }

    // 计算累计均价——按"当日"分组（跨日重置累计）
    let mut points: Vec<MinutePoint> = Vec::with_capacity(bars.len());
    let mut cum_amount: f64 = 0.0;
    let mut cum_volume: f64 = 0.0;
    let mut last_date: (u16, u8, u8) = (0, 0, 0);
    for b in bars.iter() {
        let date_key = (b.year, b.month, b.day);
        if date_key != last_date {
            cum_amount = 0.0;
            cum_volume = 0.0;
            last_date = date_key;
        }
        cum_amount += b.amount;
        cum_volume += b.volume;
        let avg = if cum_volume > 0.0 {
            // volume 单位"手" → 股需 ×100；但下面 amount / (volume×100) 才是元/股
            // EM trends2 的 average 字段单位是"元/股"——同口径
            Some(Yuan::from_unchecked(cum_amount / (cum_volume * 100.0)))
        } else {
            None
        };
        let time = format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            b.year, b.month, b.day, b.hour, b.minute
        );
        points.push(MinutePoint {
            time,
            price: Yuan::from_unchecked(b.close),
            average: avg,
            volume: Some(Lots::from_unchecked(b.volume as i64)),
            amount: Some(Yuan::from_unchecked(b.amount)),
        });
    }
    Ok(points)
}

fn beijing_date_from_ms(ms: i64) -> Option<TradeDate> {
    let utc = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)?;
    let beijing = utc + chrono::Duration::hours(8);
    use chrono::Datelike;
    TradeDate::new(
        beijing.year() * 10000 + beijing.month() as i32 * 100 + beijing.day() as i32,
    )
    .ok()
}
