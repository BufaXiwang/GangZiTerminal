//! TDX adapter — 把协议层的原始 `SecurityQuote` / `Bar` 翻译到 domain canonical model。
//!
//! Spec: docs/design/quotes-module.md §5；docs/design/references/quotes/tdx.md
//!
//! - 只支持 SH / SZ；BJ 标的不发到 TDX，由调用方走 Eastmoney fallback。
//! - 同步 TCP；async wrapper 使用 `tokio::task::spawn_blocking`。
//! - 失败后丢弃 client；下次重连（spec §5 reference 规则）。

use super::{Bar, BarCategory, MinuteTimePoint, TdxHqClient, TdxMarket};
use crate::domain::quotes::{
    KlinePeriod, KlinePoint, MinuteKlinePeriod, MinuteKlinePoint, QuoteDepthLevel, QuoteSource,
    StockQuote, TradeStatus,
};
use crate::domain::shared::{
    Amount, FreshnessStatus, InstrumentCategory, Market, OccurredAt, Price, TimestampMs, TradeDate,
    TsCode, Volume,
};
use chrono::{NaiveDate, TimeZone, Utc};
use chrono_tz::Asia::Shanghai;
use rust_decimal::{prelude::FromPrimitive, Decimal};
use std::time::Duration;

/// 用 `TsCode` 派生 TDX market 参数。BJ 不支持。
pub fn tdx_market_for(ts_code: &TsCode) -> Option<TdxMarket> {
    match ts_code.market() {
        Market::SH => Some(TdxMarket::SH),
        Market::SZ => Some(TdxMarket::SZ),
        Market::BJ => None,
    }
}

/// 把 6 位代码切出来。
fn code6(ts_code: &TsCode) -> &str {
    &ts_code.as_str()[..6]
}

/// 把 TDX `SecurityQuote` 翻译成 canonical `StockQuote`。
///
/// 调用方负责：
/// - 已知 `TsCode` 和 `InstrumentCategory`（不从 TDX 推断）。
/// - 已知请求时刻 `now`（用于 `capturedAt`）。
/// - 已知 eligible `tradeDate`（由 `MarketTimeContext` 派生）。
///
/// 输出 freshness 仅填 `source = "tdx"` 占位；最终 freshness 由 query facade 派生。
pub fn map_security_quote(
    raw: &super::SecurityQuote,
    ts_code: TsCode,
    category: InstrumentCategory,
    name: Option<String>,
    trade_date: TradeDate,
    now: OccurredAt,
) -> StockQuote {
    let price = f64_to_price(raw.price);
    let prev = f64_to_price(raw.last_close);
    let open = f64_to_price(raw.open);
    let high = f64_to_price(raw.high);
    let low = f64_to_price(raw.low);
    let volume = if raw.vol > 0.0 {
        Some(Volume(raw.vol as i64))
    } else {
        None
    };
    let amount = if raw.amount > 0.0 {
        f64_to_amount(raw.amount)
    } else {
        None
    };
    let change = match (price, prev) {
        (Some(p), Some(pc)) => Some(Price(p.0 - pc.0)),
        _ => None,
    };
    let change_percent = match (price, prev) {
        (Some(p), Some(pc)) if pc.0 > Decimal::ZERO => {
            let pct = ((p.0 - pc.0) / pc.0 * Decimal::from(100)).round_dp(4);
            pct.to_string().parse::<f64>().ok()
        }
        _ => None,
    };

    // 五档盘口
    let mut bid: Vec<QuoteDepthLevel> = Vec::with_capacity(5);
    let mut ask: Vec<QuoteDepthLevel> = Vec::with_capacity(5);
    for level in raw.book.iter() {
        bid.push(QuoteDepthLevel {
            price: f64_to_price(level.bid),
            volume: if level.bid_vol > 0.0 {
                Some(Volume(level.bid_vol as i64))
            } else {
                None
            },
        });
        ask.push(QuoteDepthLevel {
            price: f64_to_price(level.ask),
            volume: if level.ask_vol > 0.0 {
                Some(Volume(level.ask_vol as i64))
            } else {
                None
            },
        });
    }

    StockQuote {
        ts_code,
        name,
        category,
        trade_date,
        price,
        previous_close: prev,
        open,
        high,
        low,
        change,
        change_percent,
        volume,
        amount,
        turnover_rate: None,
        volume_ratio: None,
        limit_up: None,
        limit_down: None,
        bid,
        ask,
        // adapter 不派生 tradeStatus（spec §2/§5：query facade 派生）；先标 Unknown。
        trade_status: TradeStatus::Unknown,
        source: QuoteSource::Tdx,
        captured_at: now,
        exchange_time: None,
        freshness: crate::domain::shared::Freshness {
            status: FreshnessStatus::Fresh,
            captured_at: Some(now),
            exchange_time: None,
            age_ms: Some(0),
            source: Some("tdx".to_string()),
            warning: None,
        },
        warnings: Vec::new(),
    }
}

fn f64_to_price(v: f64) -> Option<Price> {
    if v.is_finite() && v > 0.0 {
        Decimal::from_f64(v).map(|d| Price(d.round_dp(4)))
    } else {
        None
    }
}

fn f64_to_amount(v: f64) -> Option<Amount> {
    if v.is_finite() && v > 0.0 {
        Decimal::from_f64(v).map(|d| Amount(d.round_dp(4)))
    } else {
        None
    }
}

/// 把 TDX `Bar`（日 K）映射成 `KlinePoint`，`adjust = none`。
pub fn map_daily_bar(b: &Bar) -> Option<KlinePoint> {
    let date = NaiveDate::from_ymd_opt(b.year as i32, b.month as u32, b.day as u32)?;
    let date = TradeDate::from_naive(date);
    Some(KlinePoint {
        date,
        open: f64_to_price(b.open)?,
        close: f64_to_price(b.close)?,
        high: f64_to_price(b.high)?,
        low: f64_to_price(b.low)?,
        volume: if b.volume > 0.0 {
            Some(Volume(b.volume as i64))
        } else {
            None
        },
        amount: f64_to_amount(b.amount),
    })
}

/// 把 TDX `Bar`（分钟）映射成 `MinuteKlinePoint`。
pub fn map_minute_bar(b: &Bar) -> Option<MinuteKlinePoint> {
    let nd = NaiveDate::from_ymd_opt(b.year as i32, b.month as u32, b.day as u32)?;
    let dt = nd.and_hms_opt(b.hour as u32, b.minute as u32, 0)?;
    let utc = Shanghai
        .from_local_datetime(&dt)
        .single()?
        .with_timezone(&Utc);
    let timestamp_ms: TimestampMs = utc.timestamp_millis();
    Some(MinuteKlinePoint {
        timestamp_ms,
        open: f64_to_price(b.open)?,
        close: f64_to_price(b.close)?,
        high: f64_to_price(b.high)?,
        low: f64_to_price(b.low)?,
        volume: if b.volume > 0.0 {
            Volume(b.volume as i64)
        } else {
            Volume(0)
        },
        amount: f64_to_amount(b.amount).unwrap_or(Amount(Decimal::ZERO)),
    })
}

/// 把 TDX 协议层 `XdxrRecord` 翻译到 domain `XdxrEvent`。
///
/// Spec: docs/design/quotes-module.md §2 "本地复权计算（基于 TDX xdxr）"。
///
/// 不可识别 category（不在 1..=14 之内）返回 None；调用方应 skip。
/// 各 category 按 spec 注释填充对应字段（非 1/11/12 的字段子集对复权无影响，但保留供未来用）。
pub fn tdx_xdxr_to_domain(
    ts_code: &TsCode,
    record: &super::XdxrRecord,
    fetched_at: TimestampMs,
) -> Option<crate::domain::quotes::XdxrEvent> {
    use crate::domain::quotes::{XdxrCategory, XdxrEvent};
    let date = NaiveDate::from_ymd_opt(
        record.year as i32,
        record.month as u32,
        record.day as u32,
    )?;
    let occur_date = TradeDate::from_naive(date);
    let category = XdxrCategory::from_u8(record.category)?;
    Some(XdxrEvent {
        ts_code: ts_code.clone(),
        occur_date,
        category,
        fenhong: record.fenhong.map(|v| v as f64),
        peigujia: record.peigujia.map(|v| v as f64),
        songzhuangu: record.songzhuangu.map(|v| v as f64),
        peigu: record.peigu.map(|v| v as f64),
        suogu: record.suogu.map(|v| v as f64),
        xingquanjia: record.xingquanjia.map(|v| v as f64),
        fenshu: record.fenshu.map(|v| v as f64),
        panqianliutong: record.panqianliutong,
        qianzongguben: record.qianzongguben,
        panhouliutong: record.panhouliutong,
        houzongguben: record.houzongguben,
        fetched_at,
    })
}

/// 把 TDX `MinuteTimePoint` 序列翻译成 domain `quote_intraday` repo 的 upsert tuple
/// `(time, price, volume, amount)`，时间按 index 派生（spec §5 line 820 + minute_time 协议）。
///
/// pytdx `get_minute_time_data` 返回 240 个点对应 A 股标准连续竞价时段的每分钟：
/// - 上午 120 点：09:30 → 11:29（含端点逐分钟）
/// - 下午 120 点：13:00 → 14:59
///
/// 一些行情服务器实测会返回 241 点（多出 15:00 收盘集合竞价末端价格）或 242 点；
/// 本适配器按"前 240 点 → 标准 240 槽位"映射，多余点 append 在 14:59 之后向 15:00 方向递推。
/// 点数 < 240 时按已有 N 点对齐前 N 个槽位，剩余分钟该 `time` 缺席。
///
/// `price` 用 `f64_to_price` 校验（>0 + finite）；不合法点 skip。
/// `volume` 直接转换（已是 i64）；`amount` 不由协议返回，置 None。
pub fn tdx_minute_time_to_intraday_points(
    points: &[MinuteTimePoint],
) -> Vec<(String, Price, Option<Volume>, Option<Amount>)> {
    let slots = trading_minute_slots();
    let mut out: Vec<(String, Price, Option<Volume>, Option<Amount>)> =
        Vec::with_capacity(points.len());
    for (idx, pt) in points.iter().enumerate() {
        let time = if idx < slots.len() {
            slots[idx].to_string()
        } else {
            // > 240 点：第 241 点视为 15:00 收盘集合竞价末端，后续逐分钟延伸（实际极少出现）。
            let extra = idx - slots.len(); // 0 → 15:00, 1 → 15:01 ...
            let m = 15u32 * 60 + extra as u32;
            format!("{:02}:{:02}", m / 60, m % 60)
        };
        let Some(price) = f64_to_price(pt.price) else {
            continue;
        };
        let volume = if pt.volume > 0 {
            Some(Volume(pt.volume))
        } else {
            None
        };
        out.push((time, price, volume, None));
    }
    out
}

/// A 股连续竞价 240 个分钟槽位（北京时间 `HH:MM`，升序）。
///
/// 09:30 ~ 11:29（120 个） + 13:00 ~ 14:59（120 个）= 240。
fn trading_minute_slots() -> Vec<String> {
    let mut slots = Vec::with_capacity(240);
    // 上午 09:30 ~ 11:29
    for m in 0..120u32 {
        let total = 9 * 60 + 30 + m;
        slots.push(format!("{:02}:{:02}", total / 60, total % 60));
    }
    // 下午 13:00 ~ 14:59
    for m in 0..120u32 {
        let total = 13 * 60 + m;
        slots.push(format!("{:02}:{:02}", total / 60, total % 60));
    }
    slots
}

pub fn kline_period_to_tdx(p: KlinePeriod) -> BarCategory {
    match p {
        KlinePeriod::Day => BarCategory::Day,
        KlinePeriod::Week => BarCategory::Week,
        KlinePeriod::Month => BarCategory::Month,
    }
}

pub fn minute_period_to_tdx(p: MinuteKlinePeriod) -> BarCategory {
    match p {
        MinuteKlinePeriod::M1 => BarCategory::Minute1,
        MinuteKlinePeriod::M5 => BarCategory::Minute5,
        MinuteKlinePeriod::M15 => BarCategory::Minute15,
        MinuteKlinePeriod::M30 => BarCategory::Minute30,
        MinuteKlinePeriod::M60 => BarCategory::Hour,
    }
}

/// TDX 默认 connect timeout（spec reference §默认限制 5s）。
pub const TDX_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// TDX quote 批量上限（spec reference §默认限制 80）。
pub const TDX_QUOTE_BATCH_MAX: usize = 80;
/// TDX K 线单次上限（spec reference 800）。
pub const TDX_BARS_MAX: u16 = 800;

/// 异步 wrapper：批量取报价。失败后调用方应丢弃 client（spec §5）。
///
/// 调用方持有 `&mut TdxHqClient`；spawn_blocking 不便用，所以本函数接受 client by-value 并返回。
pub async fn async_security_quotes(
    mut client: TdxHqClient,
    requests: Vec<(TdxMarket, String)>,
) -> (TdxHqClient, super::Result<Vec<super::SecurityQuote>>) {
    let res = tokio::task::spawn_blocking(move || {
        let refs: Vec<(TdxMarket, &str)> =
            requests.iter().map(|(m, c)| (*m, c.as_str())).collect();
        let out = client.security_quotes(&refs);
        (client, out)
    })
    .await
    .expect("spawn_blocking join failed");
    res
}

/// 异步 wrapper：取 K 线。
pub async fn async_security_bars(
    mut client: TdxHqClient,
    category: BarCategory,
    market: TdxMarket,
    code: String,
    start: u16,
    count: u16,
) -> (TdxHqClient, super::Result<Vec<Bar>>) {
    let res = tokio::task::spawn_blocking(move || {
        let out = client.security_bars(category, market, &code, start, count);
        (client, out)
    })
    .await
    .expect("spawn_blocking join failed");
    res
}

/// 异步 wrapper：取 security 列表（universe bootstrap）。
pub async fn async_security_list(
    mut client: TdxHqClient,
    market: TdxMarket,
    start: u16,
) -> (TdxHqClient, super::Result<Vec<super::SecurityListEntry>>) {
    let res = tokio::task::spawn_blocking(move || {
        let out = client.security_list(market, start);
        (client, out)
    })
    .await
    .expect("spawn_blocking join failed");
    res
}

/// 异步 wrapper：建立 client（race best ip）。
pub async fn async_connect_bestip(timeout: Duration) -> super::Result<TdxHqClient> {
    tokio::task::spawn_blocking(move || TdxHqClient::connect_bestip(timeout).map(|(c, _)| c))
        .await
        .expect("spawn_blocking join failed")
}

#[allow(dead_code)]
#[doc(hidden)]
fn _use_helper(b: &Bar) -> Option<KlinePoint> {
    let _ = code6;
    map_daily_bar(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trading_minute_slots_count_240() {
        let s = trading_minute_slots();
        assert_eq!(s.len(), 240);
        assert_eq!(s.first().unwrap(), "09:30");
        // 上午最后一槽 = 11:29
        assert_eq!(s[119], "11:29");
        // 下午第一槽 = 13:00
        assert_eq!(s[120], "13:00");
        // 下午最后一槽 = 14:59
        assert_eq!(s.last().unwrap(), "14:59");
    }

    #[test]
    fn minute_time_adapter_maps_first_three_slots() {
        let pts = vec![
            MinuteTimePoint {
                price: 10.0,
                volume: 100,
            },
            MinuteTimePoint {
                price: 10.5,
                volume: 200,
            },
            MinuteTimePoint {
                price: 10.6,
                volume: 0, // 无成交 → volume None
            },
        ];
        let out = tdx_minute_time_to_intraday_points(&pts);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].0, "09:30");
        assert_eq!(out[1].0, "09:31");
        assert_eq!(out[2].0, "09:32");
        assert_eq!(out[2].2, None);
        assert!(out[3..].iter().next().is_none());
    }

    #[test]
    fn minute_time_adapter_skips_nonfinite_or_nonpositive_price() {
        let pts = vec![
            MinuteTimePoint {
                price: 0.0,
                volume: 100,
            },
            MinuteTimePoint {
                price: f64::NAN,
                volume: 100,
            },
            MinuteTimePoint {
                price: 11.0,
                volume: 100,
            },
        ];
        let out = tdx_minute_time_to_intraday_points(&pts);
        // 前两点被 f64_to_price 过滤；第 3 点 idx=2 → "09:32"
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "09:32");
    }

    #[test]
    fn minute_time_adapter_full_240_points() {
        // 模拟 TDX 返回 240 点 → 全部覆盖 9:30-14:59 槽位
        let pts: Vec<MinuteTimePoint> = (0..240)
            .map(|i| MinuteTimePoint {
                price: 10.0 + (i as f64) * 0.001,
                volume: 100,
            })
            .collect();
        let out = tdx_minute_time_to_intraday_points(&pts);
        assert_eq!(out.len(), 240);
        assert_eq!(out.first().unwrap().0, "09:30");
        assert_eq!(out.last().unwrap().0, "14:59");
        // 中点：午休前后过渡
        assert_eq!(out[119].0, "11:29");
        assert_eq!(out[120].0, "13:00");
    }

    #[test]
    fn minute_time_adapter_handles_241_point_overflow() {
        // 241 点：第 241 个映射到 15:00（多余点向后延伸）
        let pts: Vec<MinuteTimePoint> = (0..241)
            .map(|i| MinuteTimePoint {
                price: 10.0 + (i as f64) * 0.001,
                volume: 1,
            })
            .collect();
        let out = tdx_minute_time_to_intraday_points(&pts);
        assert_eq!(out.len(), 241);
        assert_eq!(out[240].0, "15:00");
    }
}
