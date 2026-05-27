//! TDX adapter — 把协议层的原始 `SecurityQuote` / `Bar` 翻译到 domain canonical model。
//!
//! Spec: docs/design/quotes-module.md §5；docs/design/references/quotes/tdx.md
//!
//! - 只支持 SH / SZ；BJ 标的不发到 TDX，由调用方走 Eastmoney fallback。
//! - 同步 TCP；async wrapper 使用 `tokio::task::spawn_blocking`。
//! - 失败后丢弃 client；下次重连（spec §5 reference 规则）。

use super::{Bar, BarCategory, TdxHqClient, TdxMarket};
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
