//! TDX 连接管理器 — 连接池 + 失败重连 + per-call 调用串行化。
//!
//! Spec: docs/design/quotes-module.md §5；docs/design/references/quotes/tdx.md
//!
//! 设计：
//! - 单一连接，外加 `Mutex` 串行化（TDX 协议本身串行，单连接 + Mutex 足够并满足 spec §5
//!   "复用连接 / 失败重连"）。
//! - 每次调用都通过 spawn_blocking 跑同步 TCP；失败后丢弃连接，下次自动重连。
//! - 最大重试次数：1 次 reconnect + 1 次 retry，避免线程卡死。
//! - per-IP 速率限制：调用之间最小间隔 `MIN_CALL_INTERVAL`。

use super::{
    Bar, BarCategory, MinuteTimePoint, SecurityListEntry, SecurityQuote, TdxHqClient, TdxMarket,
    XdxrRecord,
};
use crate::domain::quotes::{MinuteKlinePoint, QuoteSource, StockQuote, TradeStatus};
use crate::domain::shared::{
    Amount, Freshness, FreshnessStatus, InstrumentCategory, OccurredAt, Price, TradeDate, TsCode,
    Volume,
};
use chrono::Utc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::task;

const MIN_CALL_INTERVAL: Duration = Duration::from_millis(80);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const QUOTE_BATCH_MAX: usize = 80;
const BARS_MAX: u16 = 800;

#[derive(Debug, Error)]
pub enum TdxManagerError {
    #[error("tdx not connected and reconnect failed: {0}")]
    Reconnect(String),
    #[error("tdx unsupported market (BJ)")]
    UnsupportedMarket,
    #[error("tdx protocol error: {0}")]
    Protocol(String),
}

struct State {
    client: Option<TdxHqClient>,
    last_call: Option<Instant>,
}

#[derive(Clone)]
pub struct TdxConnectionManager {
    inner: Arc<Mutex<State>>,
}

impl Default for TdxConnectionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TdxConnectionManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(State {
                client: None,
                last_call: None,
            })),
        }
    }

    /// 拉单只标的实时报价。失败后丢弃连接。
    pub async fn fetch_quote(
        &self,
        ts_code: &TsCode,
        category: InstrumentCategory,
        trade_date: TradeDate,
        now: OccurredAt,
        name: Option<String>,
    ) -> Result<StockQuote, TdxManagerError> {
        let market = match ts_code.market() {
            crate::domain::shared::Market::SH => TdxMarket::SH,
            crate::domain::shared::Market::SZ => TdxMarket::SZ,
            crate::domain::shared::Market::BJ => return Err(TdxManagerError::UnsupportedMarket),
        };
        let code = ts_code.as_str()[..6].to_string();
        let inner = Arc::clone(&self.inner);
        let result = task::spawn_blocking(move || {
            let mut guard = inner.lock().expect("tdx state poisoned");
            // 速率：保持最小间隔。
            if let Some(last) = guard.last_call {
                let elapsed = last.elapsed();
                if elapsed < MIN_CALL_INTERVAL {
                    std::thread::sleep(MIN_CALL_INTERVAL - elapsed);
                }
            }
            for attempt in 0..2 {
                if guard.client.is_none() {
                    match TdxHqClient::connect_bestip(CONNECT_TIMEOUT) {
                        Ok((c, _)) => guard.client = Some(c),
                        Err(e) => {
                            if attempt == 1 {
                                return Err(TdxManagerError::Reconnect(e.to_string()));
                            }
                            continue;
                        }
                    }
                }
                let cli = guard.client.as_mut().expect("client present");
                let res = cli.security_quotes(&[(market, code.as_str())]);
                guard.last_call = Some(Instant::now());
                match res {
                    Ok(list) => return Ok(list.into_iter().next()),
                    Err(e) => {
                        // 丢弃连接重连
                        guard.client = None;
                        if attempt == 1 {
                            return Err(TdxManagerError::Protocol(e.to_string()));
                        }
                    }
                }
            }
            Err(TdxManagerError::Reconnect("retries exhausted".into()))
        })
        .await
        .map_err(|e| TdxManagerError::Protocol(format!("join failed: {e}")))??;
        let Some(raw) = result else {
            return Err(TdxManagerError::Protocol("empty quote".into()));
        };
        Ok(map_security_quote(&raw, ts_code.clone(), category, name, trade_date, now))
    }

    /// 批量取报价。
    pub async fn fetch_quotes(
        &self,
        codes: Vec<(TsCode, InstrumentCategory, Option<String>)>,
        trade_date: TradeDate,
        now: OccurredAt,
    ) -> Vec<Result<StockQuote, TdxManagerError>> {
        let mut out: Vec<Result<StockQuote, TdxManagerError>> = Vec::with_capacity(codes.len());
        for chunk in codes.chunks(QUOTE_BATCH_MAX) {
            let mut pairs: Vec<(TdxMarket, String, TsCode, InstrumentCategory, Option<String>)> =
                Vec::with_capacity(chunk.len());
            for (ts, cat, name) in chunk {
                let m = match ts.market() {
                    crate::domain::shared::Market::SH => TdxMarket::SH,
                    crate::domain::shared::Market::SZ => TdxMarket::SZ,
                    crate::domain::shared::Market::BJ => {
                        out.push(Err(TdxManagerError::UnsupportedMarket));
                        continue;
                    }
                };
                pairs.push((m, ts.as_str()[..6].to_string(), ts.clone(), *cat, name.clone()));
            }
            if pairs.is_empty() {
                continue;
            }
            let inner = Arc::clone(&self.inner);
            let pairs_for_call: Vec<(TdxMarket, String)> = pairs
                .iter()
                .map(|(m, c, _, _, _)| (*m, c.clone()))
                .collect();
            let raw_res = task::spawn_blocking(move || -> Result<Vec<SecurityQuote>, TdxManagerError> {
                let mut guard = inner.lock().expect("tdx state poisoned");
                if let Some(last) = guard.last_call {
                    let e = last.elapsed();
                    if e < MIN_CALL_INTERVAL {
                        std::thread::sleep(MIN_CALL_INTERVAL - e);
                    }
                }
                for attempt in 0..2 {
                    if guard.client.is_none() {
                        match TdxHqClient::connect_bestip(CONNECT_TIMEOUT) {
                            Ok((c, _)) => guard.client = Some(c),
                            Err(e) => {
                                if attempt == 1 {
                                    return Err(TdxManagerError::Reconnect(e.to_string()));
                                }
                                continue;
                            }
                        }
                    }
                    let cli = guard.client.as_mut().expect("client");
                    let refs: Vec<(TdxMarket, &str)> =
                        pairs_for_call.iter().map(|(m, c)| (*m, c.as_str())).collect();
                    let res = cli.security_quotes(&refs);
                    guard.last_call = Some(Instant::now());
                    match res {
                        Ok(v) => return Ok(v),
                        Err(e) => {
                            guard.client = None;
                            if attempt == 1 {
                                return Err(TdxManagerError::Protocol(e.to_string()));
                            }
                        }
                    }
                }
                Err(TdxManagerError::Reconnect("retries exhausted".into()))
            })
            .await
            .map_err(|e| TdxManagerError::Protocol(format!("join: {e}")));
            let raws = match raw_res {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => {
                    for (_, _, ts, _, _) in &pairs {
                        let _ = ts;
                        out.push(Err(match &e {
                            TdxManagerError::UnsupportedMarket => TdxManagerError::UnsupportedMarket,
                            TdxManagerError::Reconnect(m) => TdxManagerError::Reconnect(m.clone()),
                            TdxManagerError::Protocol(m) => TdxManagerError::Protocol(m.clone()),
                        }));
                    }
                    continue;
                }
                Err(e) => {
                    for _ in &pairs {
                        out.push(Err(TdxManagerError::Protocol(e.to_string())));
                    }
                    continue;
                }
            };
            // 按 (market, code) 匹配回 ts_code。
            for (_, code, ts, cat, name) in pairs {
                let raw = raws.iter().find(|q| q.code == code);
                match raw {
                    Some(r) => out.push(Ok(map_security_quote(r, ts, cat, name, trade_date, now))),
                    None => out.push(Err(TdxManagerError::Protocol("missing in response".into()))),
                }
            }
        }
        out
    }

    /// 取日 K（不复权）。
    pub async fn fetch_daily_kline(
        &self,
        ts_code: &TsCode,
        count: u16,
    ) -> Result<Vec<Bar>, TdxManagerError> {
        let market = match ts_code.market() {
            crate::domain::shared::Market::SH => TdxMarket::SH,
            crate::domain::shared::Market::SZ => TdxMarket::SZ,
            crate::domain::shared::Market::BJ => return Err(TdxManagerError::UnsupportedMarket),
        };
        let code = ts_code.as_str()[..6].to_string();
        let inner = Arc::clone(&self.inner);
        let count = count.min(BARS_MAX);
        task::spawn_blocking(move || {
            let mut guard = inner.lock().expect("tdx state poisoned");
            if let Some(last) = guard.last_call {
                let e = last.elapsed();
                if e < MIN_CALL_INTERVAL {
                    std::thread::sleep(MIN_CALL_INTERVAL - e);
                }
            }
            for attempt in 0..2 {
                if guard.client.is_none() {
                    match TdxHqClient::connect_bestip(CONNECT_TIMEOUT) {
                        Ok((c, _)) => guard.client = Some(c),
                        Err(e) => {
                            if attempt == 1 {
                                return Err(TdxManagerError::Reconnect(e.to_string()));
                            }
                            continue;
                        }
                    }
                }
                let cli = guard.client.as_mut().expect("client");
                let res = cli.security_bars(BarCategory::Day, market, &code, 0, count);
                guard.last_call = Some(Instant::now());
                match res {
                    Ok(v) => return Ok(v),
                    Err(e) => {
                        guard.client = None;
                        if attempt == 1 {
                            return Err(TdxManagerError::Protocol(e.to_string()));
                        }
                    }
                }
            }
            Err(TdxManagerError::Reconnect("retries exhausted".into()))
        })
        .await
        .map_err(|e| TdxManagerError::Protocol(format!("join: {e}")))?
    }

    /// 取日 / 周 / 月 K（spec §5：TDX 是日 / 周 / 月 K 主源）。BJ 不支持。
    ///
    /// 单次拉取根数受 TDX 协议限制，count 自动 clamp 到 `BARS_MAX` (800)。
    pub async fn fetch_kline(
        &self,
        ts_code: &TsCode,
        period: crate::domain::quotes::KlinePeriod,
        count: u16,
    ) -> Result<Vec<Bar>, TdxManagerError> {
        use crate::infrastructure::quotes::tdx::adapter::kline_period_to_tdx;
        let market = match ts_code.market() {
            crate::domain::shared::Market::SH => TdxMarket::SH,
            crate::domain::shared::Market::SZ => TdxMarket::SZ,
            crate::domain::shared::Market::BJ => return Err(TdxManagerError::UnsupportedMarket),
        };
        let code = ts_code.as_str()[..6].to_string();
        let inner = Arc::clone(&self.inner);
        let cat = kline_period_to_tdx(period);
        let count = count.min(BARS_MAX);
        task::spawn_blocking(move || {
            let mut guard = inner.lock().expect("tdx state poisoned");
            if let Some(last) = guard.last_call {
                let e = last.elapsed();
                if e < MIN_CALL_INTERVAL {
                    std::thread::sleep(MIN_CALL_INTERVAL - e);
                }
            }
            for attempt in 0..2 {
                if guard.client.is_none() {
                    match TdxHqClient::connect_bestip(CONNECT_TIMEOUT) {
                        Ok((c, _)) => guard.client = Some(c),
                        Err(e) => {
                            if attempt == 1 {
                                return Err(TdxManagerError::Reconnect(e.to_string()));
                            }
                            continue;
                        }
                    }
                }
                let cli = guard.client.as_mut().expect("client");
                let res = cli.security_bars(cat, market, &code, 0, count);
                guard.last_call = Some(Instant::now());
                match res {
                    Ok(v) => return Ok(v),
                    Err(e) => {
                        guard.client = None;
                        if attempt == 1 {
                            return Err(TdxManagerError::Protocol(e.to_string()));
                        }
                    }
                }
            }
            Err(TdxManagerError::Reconnect("retries exhausted".into()))
        })
        .await
        .map_err(|e| TdxManagerError::Protocol(format!("join: {e}")))?
    }

    /// 取分钟 K（spec §5：分钟 K 主源 TDX）。BJ 不支持。
    pub async fn fetch_minute_kline(
        &self,
        ts_code: &TsCode,
        period: crate::domain::quotes::MinuteKlinePeriod,
        count: u16,
    ) -> Result<Vec<Bar>, TdxManagerError> {
        use crate::infrastructure::quotes::tdx::adapter::minute_period_to_tdx;
        let market = match ts_code.market() {
            crate::domain::shared::Market::SH => TdxMarket::SH,
            crate::domain::shared::Market::SZ => TdxMarket::SZ,
            crate::domain::shared::Market::BJ => return Err(TdxManagerError::UnsupportedMarket),
        };
        let code = ts_code.as_str()[..6].to_string();
        let inner = Arc::clone(&self.inner);
        let cat = minute_period_to_tdx(period);
        let count = count.min(BARS_MAX);
        task::spawn_blocking(move || {
            let mut guard = inner.lock().expect("tdx state poisoned");
            if let Some(last) = guard.last_call {
                let e = last.elapsed();
                if e < MIN_CALL_INTERVAL {
                    std::thread::sleep(MIN_CALL_INTERVAL - e);
                }
            }
            for attempt in 0..2 {
                if guard.client.is_none() {
                    match TdxHqClient::connect_bestip(CONNECT_TIMEOUT) {
                        Ok((c, _)) => guard.client = Some(c),
                        Err(e) => {
                            if attempt == 1 {
                                return Err(TdxManagerError::Reconnect(e.to_string()));
                            }
                            continue;
                        }
                    }
                }
                let cli = guard.client.as_mut().expect("client");
                let res = cli.security_bars(cat, market, &code, 0, count);
                guard.last_call = Some(Instant::now());
                match res {
                    Ok(v) => return Ok(v),
                    Err(e) => {
                        guard.client = None;
                        if attempt == 1 {
                            return Err(TdxManagerError::Protocol(e.to_string()));
                        }
                    }
                }
            }
            Err(TdxManagerError::Reconnect("retries exhausted".into()))
        })
        .await
        .map_err(|e| TdxManagerError::Protocol(format!("join: {e}")))?
    }

    /// 除权除息 / 公司行动历史。BJ 不支持。
    pub async fn fetch_xdxr(
        &self,
        ts_code: &TsCode,
    ) -> Result<Vec<XdxrRecord>, TdxManagerError> {
        let market = match ts_code.market() {
            crate::domain::shared::Market::SH => TdxMarket::SH,
            crate::domain::shared::Market::SZ => TdxMarket::SZ,
            crate::domain::shared::Market::BJ => return Err(TdxManagerError::UnsupportedMarket),
        };
        let code = ts_code.as_str()[..6].to_string();
        let inner = Arc::clone(&self.inner);
        task::spawn_blocking(move || {
            let mut guard = inner.lock().expect("tdx state poisoned");
            if let Some(last) = guard.last_call {
                let e = last.elapsed();
                if e < MIN_CALL_INTERVAL {
                    std::thread::sleep(MIN_CALL_INTERVAL - e);
                }
            }
            for attempt in 0..2 {
                if guard.client.is_none() {
                    match TdxHqClient::connect_bestip(CONNECT_TIMEOUT) {
                        Ok((c, _)) => guard.client = Some(c),
                        Err(e) => {
                            if attempt == 1 {
                                return Err(TdxManagerError::Reconnect(e.to_string()));
                            }
                            continue;
                        }
                    }
                }
                let cli = guard.client.as_mut().expect("client");
                let res = cli.security_xdxr(market, &code);
                guard.last_call = Some(Instant::now());
                match res {
                    Ok(v) => return Ok(v),
                    Err(e) => {
                        guard.client = None;
                        if attempt == 1 {
                            return Err(TdxManagerError::Protocol(e.to_string()));
                        }
                    }
                }
            }
            Err(TdxManagerError::Reconnect("retries exhausted".into()))
        })
        .await
        .map_err(|e| TdxManagerError::Protocol(format!("join: {e}")))?
    }

    /// 当日分时（240 个交易分钟）。BJ 不支持。
    ///
    /// 协议层返回的 `MinuteTimePoint` 不含时间戳——index 对应交易时段第 N 分钟。
    /// pipeline / domain 层负责派生具体时间。
    pub async fn fetch_minute_time(
        &self,
        ts_code: &TsCode,
    ) -> Result<Vec<MinuteTimePoint>, TdxManagerError> {
        let market = match ts_code.market() {
            crate::domain::shared::Market::SH => TdxMarket::SH,
            crate::domain::shared::Market::SZ => TdxMarket::SZ,
            crate::domain::shared::Market::BJ => return Err(TdxManagerError::UnsupportedMarket),
        };
        let code = ts_code.as_str()[..6].to_string();
        let inner = Arc::clone(&self.inner);
        task::spawn_blocking(move || {
            let mut guard = inner.lock().expect("tdx state poisoned");
            if let Some(last) = guard.last_call {
                let e = last.elapsed();
                if e < MIN_CALL_INTERVAL {
                    std::thread::sleep(MIN_CALL_INTERVAL - e);
                }
            }
            for attempt in 0..2 {
                if guard.client.is_none() {
                    match TdxHqClient::connect_bestip(CONNECT_TIMEOUT) {
                        Ok((c, _)) => guard.client = Some(c),
                        Err(e) => {
                            if attempt == 1 {
                                return Err(TdxManagerError::Reconnect(e.to_string()));
                            }
                            continue;
                        }
                    }
                }
                let cli = guard.client.as_mut().expect("client");
                let res = cli.security_minute_time(market, &code);
                guard.last_call = Some(Instant::now());
                match res {
                    Ok(v) => return Ok(v),
                    Err(e) => {
                        guard.client = None;
                        if attempt == 1 {
                            return Err(TdxManagerError::Protocol(e.to_string()));
                        }
                    }
                }
            }
            Err(TdxManagerError::Reconnect("retries exhausted".into()))
        })
        .await
        .map_err(|e| TdxManagerError::Protocol(format!("join: {e}")))?
    }

    /// 拉取 SH 或 SZ 全 universe（分页）。
    ///
    /// Spec: quotes-module.md §5 line 728 — TDX 是 SH / SZ universe 的主源。
    /// `security_count` 给出总数；`security_list(market, start)` 每次返回最多 1000 条；
    /// 调用方根据 6 位 code 前缀分类为 stock / index / fund。
    pub async fn fetch_universe(
        &self,
        market: TdxMarket,
    ) -> Result<Vec<SecurityListEntry>, TdxManagerError> {
        let inner = Arc::clone(&self.inner);
        task::spawn_blocking(move || {
            let mut guard = inner.lock().expect("tdx state poisoned");
            if let Some(last) = guard.last_call {
                let e = last.elapsed();
                if e < MIN_CALL_INTERVAL {
                    std::thread::sleep(MIN_CALL_INTERVAL - e);
                }
            }
            // 确保连接
            for attempt in 0..2 {
                if guard.client.is_none() {
                    match TdxHqClient::connect_bestip(CONNECT_TIMEOUT) {
                        Ok((c, _)) => guard.client = Some(c),
                        Err(e) => {
                            if attempt == 1 {
                                return Err(TdxManagerError::Reconnect(e.to_string()));
                            }
                            continue;
                        }
                    }
                }
                break;
            }
            if guard.client.is_none() {
                return Err(TdxManagerError::Reconnect("client not established".into()));
            }
            // 先拿总数
            let count = {
                let cli = guard.client.as_mut().expect("client present");
                let r = cli.security_count(market);
                guard.last_call = Some(Instant::now());
                match r {
                    Ok(n) => n,
                    Err(e) => {
                        guard.client = None;
                        return Err(TdxManagerError::Protocol(e.to_string()));
                    }
                }
            };
            let mut out: Vec<SecurityListEntry> = Vec::with_capacity(count as usize);
            let mut start: u16 = 0;
            const PAGE: u16 = 1000;
            while start < count {
                if let Some(last) = guard.last_call {
                    let e = last.elapsed();
                    if e < MIN_CALL_INTERVAL {
                        std::thread::sleep(MIN_CALL_INTERVAL - e);
                    }
                }
                let res = {
                    let cli = guard.client.as_mut().expect("client present");
                    cli.security_list(market, start)
                };
                guard.last_call = Some(Instant::now());
                match res {
                    Ok(mut page) => {
                        if page.is_empty() {
                            break;
                        }
                        let got = page.len() as u16;
                        out.append(&mut page);
                        // 防止 wrap：用实际读取条数推进 start
                        start = start.saturating_add(if got > 0 { got } else { PAGE });
                        if got < PAGE {
                            break;
                        }
                    }
                    Err(e) => {
                        guard.client = None;
                        return Err(TdxManagerError::Protocol(e.to_string()));
                    }
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| TdxManagerError::Protocol(format!("join: {e}")))?
    }
}

fn map_security_quote(
    raw: &SecurityQuote,
    ts_code: TsCode,
    category: InstrumentCategory,
    name: Option<String>,
    trade_date: TradeDate,
    now: OccurredAt,
) -> StockQuote {
    use rust_decimal::{prelude::FromPrimitive, Decimal};
    let f64_to_price = |v: f64| -> Option<Price> {
        if v.is_finite() && v > 0.0 {
            Decimal::from_f64(v).map(|d| Price(d.round_dp(4)))
        } else {
            None
        }
    };
    let f64_to_amount = |v: f64| -> Option<Amount> {
        if v.is_finite() && v > 0.0 {
            Decimal::from_f64(v).map(|d| Amount(d.round_dp(4)))
        } else {
            None
        }
    };
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
    let mut bid: Vec<crate::domain::quotes::QuoteDepthLevel> = Vec::with_capacity(5);
    let mut ask: Vec<crate::domain::quotes::QuoteDepthLevel> = Vec::with_capacity(5);
    for level in raw.book.iter() {
        bid.push(crate::domain::quotes::QuoteDepthLevel {
            price: f64_to_price(level.bid),
            volume: if level.bid_vol > 0.0 {
                Some(Volume(level.bid_vol as i64))
            } else {
                None
            },
        });
        ask.push(crate::domain::quotes::QuoteDepthLevel {
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
        trade_status: TradeStatus::Unknown,
        source: QuoteSource::Tdx,
        captured_at: now,
        exchange_time: None,
        freshness: Freshness {
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

/// 把 TDX 分钟 Bar 翻译为 MinuteKlinePoint。
pub fn map_minute_bar(b: &Bar) -> Option<MinuteKlinePoint> {
    use chrono::TimeZone;
    use chrono_tz::Asia::Shanghai;
    use rust_decimal::{prelude::FromPrimitive, Decimal};
    let nd = chrono::NaiveDate::from_ymd_opt(b.year as i32, b.month as u32, b.day as u32)?;
    let dt = nd.and_hms_opt(b.hour as u32, b.minute as u32, 0)?;
    let utc = Shanghai
        .from_local_datetime(&dt)
        .single()?
        .with_timezone(&Utc);
    let ts_ms = utc.timestamp_millis();
    let _f64_to_price = |v: f64| -> Option<Price> {
        if v.is_finite() && v > 0.0 {
            Decimal::from_f64(v).map(|d| Price(d.round_dp(4)))
        } else {
            None
        }
    };
    Some(MinuteKlinePoint {
        timestamp_ms: ts_ms,
        open: Price(Decimal::from_f64(b.open).unwrap_or_default().round_dp(4)),
        close: Price(Decimal::from_f64(b.close).unwrap_or_default().round_dp(4)),
        high: Price(Decimal::from_f64(b.high).unwrap_or_default().round_dp(4)),
        low: Price(Decimal::from_f64(b.low).unwrap_or_default().round_dp(4)),
        volume: if b.volume > 0.0 {
            Volume(b.volume as i64)
        } else {
            Volume(0)
        },
        amount: Amount(Decimal::from_f64(b.amount).unwrap_or_default().round_dp(4)),
    })
}

/// 把 TDX 日 Bar 翻译为 KlinePoint。
pub fn map_daily_bar(b: &Bar) -> Option<crate::domain::quotes::KlinePoint> {
    use chrono::NaiveDate;
    use rust_decimal::{prelude::FromPrimitive, Decimal};
    let date = NaiveDate::from_ymd_opt(b.year as i32, b.month as u32, b.day as u32)?;
    let date = TradeDate::from_naive(date);
    Some(crate::domain::quotes::KlinePoint {
        date,
        open: Price(Decimal::from_f64(b.open)?.round_dp(4)),
        close: Price(Decimal::from_f64(b.close)?.round_dp(4)),
        high: Price(Decimal::from_f64(b.high)?.round_dp(4)),
        low: Price(Decimal::from_f64(b.low)?.round_dp(4)),
        volume: if b.volume > 0.0 {
            Some(Volume(b.volume as i64))
        } else {
            None
        },
        amount: if b.amount > 0.0 {
            Some(Amount(Decimal::from_f64(b.amount)?.round_dp(4)))
        } else {
            None
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::quotes::tdx::QuoteLevel;

    fn sample_raw() -> SecurityQuote {
        SecurityQuote {
            market: 1,
            code: "600519".to_string(),
            active1: 0,
            price: 1500.0,
            last_close: 1480.0,
            open: 1490.0,
            high: 1520.0,
            low: 1485.0,
            vol: 1234567.0,
            cur_vol: 100.0,
            amount: 1_234_567_890.0,
            s_vol: 600_000.0,
            b_vol: 700_000.0,
            book: [
                QuoteLevel { bid: 1499.0, ask: 1501.0, bid_vol: 500.0, ask_vol: 600.0 },
                QuoteLevel { bid: 1498.0, ask: 1502.0, bid_vol: 100.0, ask_vol: 200.0 },
                QuoteLevel::default(),
                QuoteLevel::default(),
                QuoteLevel::default(),
            ],
            rate: 0.013,
            active2: 0,
        }
    }

    #[test]
    fn map_security_quote_populates_bid_ask_and_change() {
        let raw = sample_raw();
        let code = TsCode::parse("600519.SH").unwrap();
        let now = Utc::now();
        let td = TradeDate::parse("20260526").unwrap();
        let q = map_security_quote(&raw, code, InstrumentCategory::Stock, None, td, now);
        assert!(q.price.is_some());
        assert!(q.previous_close.is_some());
        assert_eq!(q.bid.len(), 5);
        assert_eq!(q.ask.len(), 5);
        assert!(q.bid[0].price.is_some());
        assert!(q.ask[0].price.is_some());
        assert!(q.change.is_some());
        assert!(q.change_percent.unwrap() > 1.0);
        assert_eq!(q.source, QuoteSource::Tdx);
        // adapter 不派生 warning
        assert!(q.warnings.is_empty());
        assert!(q.freshness.warning.is_none());
    }

    #[test]
    fn map_security_quote_skips_negative_price() {
        let mut raw = sample_raw();
        raw.price = 0.0;
        raw.last_close = 0.0;
        let code = TsCode::parse("600519.SH").unwrap();
        let q = map_security_quote(
            &raw,
            code,
            InstrumentCategory::Stock,
            None,
            TradeDate::parse("20260526").unwrap(),
            Utc::now(),
        );
        assert!(q.price.is_none());
        assert!(q.change.is_none());
    }

    #[test]
    fn map_daily_bar_roundtrips() {
        let b = Bar {
            year: 2026,
            month: 5,
            day: 26,
            hour: 0,
            minute: 0,
            open: 100.0,
            high: 110.0,
            low: 99.0,
            close: 105.0,
            volume: 50000.0,
            amount: 5_250_000.0,
        };
        let p = map_daily_bar(&b).unwrap();
        assert_eq!(p.date.format(), "20260526");
        assert_eq!(p.volume.unwrap().0, 50000);
    }
}
