//! Eastmoney push2 / qt 行情 HTTP client。
//!
//! Spec: docs/design/quotes-module.md §5；docs/design/references/quotes/eastmoney.md
//!
//! 设计：单 reqwest::Client 复用；timeout 5s（quote）/ 8s（kline）；retry 1 次。
//! 失败返回 `EmError`；调用方按 spec 把它翻译成 `provider_partial_failure` / item warning。

use crate::domain::quotes::{
    KlinePoint, MinuteKlinePeriod, MinuteKlinePoint, QuoteDepthLevel, QuoteSource, StockQuote,
    TradeStatus,
};
use crate::domain::shared::{
    Amount, Freshness, FreshnessStatus, InstrumentCategory, OccurredAt, Price, TimestampMs,
    TradeDate, TsCode, Volume,
};
use chrono::{NaiveDate, TimeZone, Utc};
use chrono_tz::Asia::Shanghai;
use reqwest::Client;
use rust_decimal::{prelude::FromPrimitive, Decimal};
use serde::Deserialize;
use std::time::Duration;
use thiserror::Error;

const QUOTE_TIMEOUT: Duration = Duration::from_secs(5);
const KLINE_TIMEOUT: Duration = Duration::from_secs(8);
const USER_AGENT: &str = "Mozilla/5.0 GangZi/0.1";

#[derive(Debug, Error)]
pub enum EmError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("provider returned no data")]
    Empty,
}

#[derive(Clone)]
pub struct EastmoneyProvider {
    client: Client,
}

impl EastmoneyProvider {
    pub fn new() -> reqwest::Result<Self> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(KLINE_TIMEOUT)
            .build()?;
        Ok(Self { client })
    }

    /// 拉单只标的实时行情（push2 接口）。
    ///
    /// EM secid 格式：`<market>.<code6>`，market 0 = SZ/BJ（实际 SZ=0, BJ=0?），1 = SH。
    /// 实际：SH = 1，SZ = 0，BJ = 0。BJ 需要带 `fields` 否则返回空——简化处理：所有标的
    /// 都用 push2，对 BJ 失败则上层 fallback。
    pub async fn fetch_quote(
        &self,
        ts_code: &TsCode,
        category: InstrumentCategory,
        eligible_trade_date: TradeDate,
        now: OccurredAt,
    ) -> Result<StockQuote, EmError> {
        let secid = secid_of(ts_code);
        // f43-f86 quote 基础；f19/f31/f33/f35/f37/f39 是 bid5..bid1 价；
        // f20/f32/f34/f36/f38 是 bid5..bid1 量；
        // f47 已包含成交量；
        // f31-f42（含奇偶 price/vol）是买盘；
        // ask 价（f47 是当日成交量；ask 价为 f47..f47 误）-- 正确字段：
        // ask: f47(成交量)？非；EM push2 fields：
        //   f31 bid5 price, f32 bid5 vol, f33 bid4, f34 bid4vol, f35 bid3, f36 bid3vol,
        //   f37 bid2, f38 bid2vol, f39 bid1, f40 bid1vol,
        //   f47/f48 是 量/额（已用），  ask 在 f41/f42(ask5)、f43... 但 f43=最新价 与文档冲突
        // mootdx / 实际生产代码：
        //   bid: f19(b5p),f20(b5v),f17(b4p),f18(b4v),f15(b3p),f16(b3v),f13(b2p),f14(b2v),f11(b1p),f12(b1v)
        //   ask: f21(a1p),f22(a1v),f23(a2p),f24(a2v),f25(a3p),f26(a3v),f27(a4p),f28(a4v),f29(a5p),f30(a5v)
        // 价格 * 100 同 f43 / f60。
        let url = format!(
            "https://push2.eastmoney.com/api/qt/stock/get?secid={}&fields=f43,f44,f45,f46,f47,f48,f57,f58,f60,f168,f169,f170,f86,f292,\
             f11,f12,f13,f14,f15,f16,f17,f18,f19,f20,\
             f21,f22,f23,f24,f25,f26,f27,f28,f29,f30",
            secid
        );
        let req = self.client.get(&url).timeout(QUOTE_TIMEOUT);
        let resp: QuoteResp = req
            .send()
            .await?
            .json()
            .await
            .map_err(|e| EmError::Parse(e.to_string()))?;
        let d = resp.data.ok_or(EmError::Empty)?;
        parse_quote(d, ts_code, category, eligible_trade_date, now)
    }

    /// 拉分钟 K（push2 kline）。
    pub async fn fetch_minute_kline(
        &self,
        ts_code: &TsCode,
        period: MinuteKlinePeriod,
        count: u32,
    ) -> Result<Vec<MinuteKlinePoint>, EmError> {
        let secid = secid_of(ts_code);
        let klt = match period {
            MinuteKlinePeriod::M1 => 1,
            MinuteKlinePeriod::M5 => 5,
            MinuteKlinePeriod::M15 => 15,
            MinuteKlinePeriod::M30 => 30,
            MinuteKlinePeriod::M60 => 60,
        };
        let url = format!(
            "https://push2his.eastmoney.com/api/qt/stock/kline/get?secid={}&fields1=f1,f2,f3,f4,f5,f6&fields2=f51,f52,f53,f54,f55,f56,f57,f58&klt={}&fqt=0&lmt={}",
            secid, klt, count
        );
        #[derive(Deserialize)]
        struct Resp {
            data: Option<KlineData>,
        }
        #[derive(Deserialize)]
        struct KlineData {
            #[serde(default)]
            klines: Vec<String>,
        }
        let resp: Resp = self
            .client
            .get(&url)
            .timeout(KLINE_TIMEOUT)
            .send()
            .await?
            .json()
            .await
            .map_err(|e| EmError::Parse(e.to_string()))?;
        let data = resp.data.ok_or(EmError::Empty)?;
        let mut out = Vec::with_capacity(data.klines.len());
        for line in data.klines.iter() {
            // 格式： "YYYY-MM-DD HH:MM,open,close,high,low,volume,amount,..."
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() < 7 {
                continue;
            }
            let dt = parts[0];
            let nd = if dt.len() >= 16 {
                let date_part = &dt[..10];
                let time_part = &dt[11..16];
                let nd = NaiveDate::parse_from_str(date_part, "%Y-%m-%d").ok();
                let (hh, mm) = time_part
                    .split_once(':')
                    .and_then(|(h, m)| Some((h.parse::<u32>().ok()?, m.parse::<u32>().ok()?)))
                    .unwrap_or((0, 0));
                nd.and_then(|d| d.and_hms_opt(hh, mm, 0))
            } else {
                continue;
            };
            let Some(naive) = nd else { continue };
            let Some(utc) = Shanghai.from_local_datetime(&naive).single().map(|t| t.with_timezone(&Utc)) else {
                continue;
            };
            let timestamp_ms: TimestampMs = utc.timestamp_millis();
            let parse_p = |s: &str| -> Option<Price> {
                s.parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .and_then(|v| Decimal::from_f64(v).map(|d| Price(d.round_dp(4))))
            };
            let Some(open) = parse_p(parts[1]) else { continue };
            let Some(close) = parse_p(parts[2]) else { continue };
            let Some(high) = parse_p(parts[3]) else { continue };
            let Some(low) = parse_p(parts[4]) else { continue };
            // EM minute volume 单位是手；× 100 转股。
            let volume = parts[5]
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v > 0.0)
                .map(|v| Volume((v * 100.0) as i64))
                .unwrap_or(Volume(0));
            let amount = parts[6]
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v > 0.0)
                .and_then(|v| Decimal::from_f64(v).map(|d| Amount(d.round_dp(4))))
                .unwrap_or(Amount(Decimal::ZERO));
            out.push(MinuteKlinePoint {
                timestamp_ms,
                open,
                close,
                high,
                low,
                volume,
                amount,
            });
        }
        Ok(out)
    }

    /// 当日分时（trends2 接口）。
    pub async fn fetch_intraday(
        &self,
        ts_code: &TsCode,
        trade_date: TradeDate,
    ) -> Result<Vec<(String, Price, Option<Volume>, Option<Amount>)>, EmError> {
        let secid = secid_of(ts_code);
        let url = format!(
            "https://push2his.eastmoney.com/api/qt/stock/trends2/get?secid={}&fields1=f1,f2,f3,f4,f5&fields2=f51,f53,f56,f58&iscr=0&ndays=1",
            secid
        );
        #[derive(Deserialize)]
        struct Resp {
            data: Option<TrendsData>,
        }
        #[derive(Deserialize)]
        struct TrendsData {
            #[serde(default)]
            trends: Vec<String>,
        }
        let resp: Resp = self
            .client
            .get(&url)
            .timeout(KLINE_TIMEOUT)
            .send()
            .await?
            .json()
            .await
            .map_err(|e| EmError::Parse(e.to_string()))?;
        let data = resp.data.ok_or(EmError::Empty)?;
        let _ = trade_date;
        let mut out = Vec::with_capacity(data.trends.len());
        for line in data.trends.iter() {
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() < 4 {
                continue;
            }
            // "YYYY-MM-DD HH:MM,price,volume,amount"
            let time_str = if parts[0].len() >= 16 {
                parts[0][11..16].to_string()
            } else {
                parts[0].to_string()
            };
            let Some(price) = parts[1]
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v > 0.0)
                .and_then(|v| Decimal::from_f64(v).map(|d| Price(d.round_dp(4))))
            else {
                continue;
            };
            let volume = parts[2]
                .parse::<f64>()
                .ok()
                .filter(|v| *v > 0.0)
                .map(|v| Volume((v * 100.0) as i64));
            let amount = parts[3]
                .parse::<f64>()
                .ok()
                .filter(|v| *v > 0.0)
                .and_then(|v| Decimal::from_f64(v).map(|d| Amount(d.round_dp(4))));
            out.push((time_str, price, volume, amount));
        }
        Ok(out)
    }

    /// 日线 K（用作 fallback；TuShare 不可用时）。
    pub async fn fetch_daily_kline(
        &self,
        ts_code: &TsCode,
        count: u32,
    ) -> Result<Vec<KlinePoint>, EmError> {
        let secid = secid_of(ts_code);
        let url = format!(
            "https://push2his.eastmoney.com/api/qt/stock/kline/get?secid={}&fields1=f1,f2,f3,f4,f5,f6&fields2=f51,f52,f53,f54,f55,f56,f57,f58&klt=101&fqt=0&lmt={}",
            secid, count
        );
        #[derive(Deserialize)]
        struct Resp {
            data: Option<KlineData>,
        }
        #[derive(Deserialize)]
        struct KlineData {
            #[serde(default)]
            klines: Vec<String>,
        }
        let resp: Resp = self
            .client
            .get(&url)
            .timeout(KLINE_TIMEOUT)
            .send()
            .await?
            .json()
            .await
            .map_err(|e| EmError::Parse(e.to_string()))?;
        let data = resp.data.ok_or(EmError::Empty)?;
        let mut out = Vec::with_capacity(data.klines.len());
        for line in data.klines.iter() {
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() < 7 {
                continue;
            }
            let date = match NaiveDate::parse_from_str(parts[0], "%Y-%m-%d") {
                Ok(d) => TradeDate::from_naive(d),
                Err(_) => continue,
            };
            let parse_p = |s: &str| -> Option<Price> {
                s.parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .and_then(|v| Decimal::from_f64(v).map(|d| Price(d.round_dp(4))))
            };
            let Some(open) = parse_p(parts[1]) else { continue };
            let Some(close) = parse_p(parts[2]) else { continue };
            let Some(high) = parse_p(parts[3]) else { continue };
            let Some(low) = parse_p(parts[4]) else { continue };
            let volume = parts[5]
                .parse::<f64>()
                .ok()
                .filter(|v| *v > 0.0)
                .map(|v| Volume((v * 100.0) as i64));
            let amount = parts[6]
                .parse::<f64>()
                .ok()
                .filter(|v| *v > 0.0)
                .and_then(|v| Decimal::from_f64(v).map(|d| Amount(d.round_dp(4))));
            out.push(KlinePoint {
                date,
                open,
                close,
                high,
                low,
                volume,
                amount,
            });
        }
        Ok(out)
    }

    /// 拉取 BJ universe（北交所主板）。
    ///
    /// Spec: quotes-module.md §5 line 729 — Eastmoney 补 BJ。
    /// 使用 `qt/clist/get` 的 `m:0+t:81+s:2048` 筛选；返回 (code6, name)。
    pub async fn fetch_bj_universe(&self) -> Result<Vec<(String, String)>, EmError> {
        // 单页足够 (BJ 标的总数 ~250)；pz=500 保险。
        let url = "https://82.push2.eastmoney.com/api/qt/clist/get?pn=1&pz=500\
            &fid=f12&fs=m:0+t:81+s:2048&fields=f12,f14";
        let body = self
            .client
            .get(url)
            .timeout(KLINE_TIMEOUT)
            .send()
            .await?
            .text()
            .await?;
        parse_bj_universe(&body)
    }
}

#[derive(Deserialize)]
struct BjUniverseResp {
    data: Option<BjUniverseData>,
}

#[derive(Deserialize)]
struct BjUniverseData {
    // EM clist/get 的 `diff` 在不同 host / 版本下可能是数组 `[{...}]` 或以数字字符串为键的
    // 对象 `{"0":{...},"1":{...}}`（live 2026-05-30 观测到对象形态）。兼容两种形态。
    #[serde(default, deserialize_with = "de_diff")]
    diff: Vec<BjRow>,
}

#[derive(Deserialize)]
struct BjRow {
    #[serde(default)]
    f12: Option<String>,
    #[serde(default)]
    f14: Option<String>,
}

/// 接受 `diff` 为数组或对象（数字键 map）→ 统一成 `Vec<BjRow>`。
/// 对象形态按数字键升序排序，保持与数组形态一致的稳定顺序。
fn de_diff<'de, D>(deserializer: D) -> Result<Vec<BjRow>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum DiffShape {
        Array(Vec<BjRow>),
        Map(std::collections::HashMap<String, BjRow>),
        Null,
    }
    Ok(match DiffShape::deserialize(deserializer)? {
        DiffShape::Array(v) => v,
        DiffShape::Map(m) => {
            let mut entries: Vec<(i64, BjRow)> = m
                .into_iter()
                .map(|(k, row)| (k.parse::<i64>().unwrap_or(i64::MAX), row))
                .collect();
            entries.sort_by_key(|(k, _)| *k);
            entries.into_iter().map(|(_, row)| row).collect()
        }
        DiffShape::Null => Vec::new(),
    })
}

/// 纯函数：解析 EM clist/get 响应体 → (code6, name) 列表（无 I/O，可单测）。
fn parse_bj_universe(body: &str) -> Result<Vec<(String, String)>, EmError> {
    let resp: BjUniverseResp =
        serde_json::from_str(body).map_err(|e| EmError::Parse(e.to_string()))?;
    let data = resp.data.ok_or(EmError::Empty)?;
    let mut out = Vec::with_capacity(data.diff.len());
    for r in data.diff.into_iter() {
        if let (Some(code), Some(name)) = (r.f12, r.f14) {
            if code.len() == 6 && code.chars().all(|c| c.is_ascii_digit()) {
                out.push((code, name));
            }
        }
    }
    Ok(out)
}

#[derive(Deserialize)]
struct QuoteResp {
    data: Option<QuoteData>,
}

/// EM push2 `stock/get` data 字段。价格类字段为 ×100 整数，f168/f170 为 percent×100。
#[derive(Deserialize)]
#[allow(dead_code)]
struct QuoteData {
    #[serde(default)] f43: Option<f64>, // 最新价 * 100
    #[serde(default)] f44: Option<f64>, // 最高
    #[serde(default)] f45: Option<f64>, // 最低
    #[serde(default)] f46: Option<f64>, // 今开
    #[serde(default)] f47: Option<f64>, // 成交量（手）
    #[serde(default)] f48: Option<f64>, // 成交额
    #[serde(default)] f60: Option<f64>, // 昨收 * 100
    #[serde(default)] f57: Option<String>, // code
    #[serde(default)] f58: Option<String>, // name
    #[serde(default)] f169: Option<f64>, // 涨跌额 * 100
    #[serde(default)] f170: Option<f64>, // 涨跌幅 * 100 (percent×100)
    #[serde(default)] f86: Option<i64>, // exchange time (epoch)
    #[serde(default)] f168: Option<f64>, // 换手率 * 100 (percent×100)
    // 买盘 5 档（价 * 100；量是手）
    #[serde(default)] f11: Option<f64>, #[serde(default)] f12: Option<f64>, // b1 price, b1 vol
    #[serde(default)] f13: Option<f64>, #[serde(default)] f14: Option<f64>, // b2
    #[serde(default)] f15: Option<f64>, #[serde(default)] f16: Option<f64>, // b3
    #[serde(default)] f17: Option<f64>, #[serde(default)] f18: Option<f64>, // b4
    #[serde(default)] f19: Option<f64>, #[serde(default)] f20: Option<f64>, // b5
    // 卖盘 5 档
    #[serde(default)] f21: Option<f64>, #[serde(default)] f22: Option<f64>, // a1
    #[serde(default)] f23: Option<f64>, #[serde(default)] f24: Option<f64>, // a2
    #[serde(default)] f25: Option<f64>, #[serde(default)] f26: Option<f64>, // a3
    #[serde(default)] f27: Option<f64>, #[serde(default)] f28: Option<f64>, // a4
    #[serde(default)] f29: Option<f64>, #[serde(default)] f30: Option<f64>, // a5
}

/// 纯函数：把 EM `stock/get` data 翻译成 domain `StockQuote`（无 I/O，可单测）。
fn parse_quote(
    d: QuoteData,
    ts_code: &TsCode,
    category: InstrumentCategory,
    eligible_trade_date: TradeDate,
    now: OccurredAt,
) -> Result<StockQuote, EmError> {
    let price = d.f43.and_then(em_div100);
    let prev = d.f60.and_then(em_div100);
    // Completeness guard (Spec finding #2): EM 偶尔为 BJ（及已退市/已迁移代码）返回一个
    // 占位 payload —— price/prevClose 字段为 0、name 含「已切换」类标记。这种 payload 不
    // 应被当作可用行情发出，否则会污染快照。要求至少 price 与 identity(code/name) 有效。
    // (live 2026-05-30 观测: secid=0.430047 返回 0 价 + “已切换” name；secid=2.430047 无数据。)
    let identity_ok = d.f57.as_deref().map(|c| !c.is_empty()).unwrap_or(false)
        || d.f58.as_deref().map(|n| !n.is_empty()).unwrap_or(false);
    if price.is_none() || !identity_ok {
        return Err(EmError::Empty);
    }
    let open = d.f46.and_then(em_div100);
    let high = d.f44.and_then(em_div100);
    let low = d.f45.and_then(em_div100);
    let change = d.f169.and_then(em_div100);
    let change_percent = d.f170.map(|v| v / 100.0);
    let volume = d
        .f47
        .filter(|v| *v > 0.0)
        // EM volume is in 手；A 股 1 手 = 100 股。Normalize 到股 (spec shared §2 — 不使用手).
        .map(|v| Volume((v * 100.0) as i64));
    let amount = d
        .f48
        .filter(|v| *v > 0.0)
        .and_then(|v| Decimal::from_f64(v).map(|d| Amount(d.round_dp(4))));
    let exchange_time = d
        .f86
        .filter(|t| *t > 0)
        .and_then(|t| Utc.timestamp_opt(t, 0).single());

    // 五档盘口：EM 价格 ÷100；量 ×100（手 → 股）。bid[0]=买一最接近成交，ask[0]=卖一。
    let level = |price_raw: Option<f64>, vol_raw: Option<f64>| -> QuoteDepthLevel {
        QuoteDepthLevel {
            price: price_raw.and_then(em_div100),
            volume: vol_raw
                .filter(|v| *v > 0.0)
                .map(|v| Volume((v * 100.0) as i64)),
        }
    };
    let bid = vec![
        level(d.f11, d.f12),
        level(d.f13, d.f14),
        level(d.f15, d.f16),
        level(d.f17, d.f18),
        level(d.f19, d.f20),
    ];
    let ask = vec![
        level(d.f21, d.f22),
        level(d.f23, d.f24),
        level(d.f25, d.f26),
        level(d.f27, d.f28),
        level(d.f29, d.f30),
    ];

    Ok(StockQuote {
        ts_code: ts_code.clone(),
        name: d.f58,
        category,
        trade_date: eligible_trade_date,
        price,
        previous_close: prev,
        open,
        high,
        low,
        change,
        change_percent,
        volume,
        amount,
        // EM f168 是换手率×100（live 2026-05-30: 600519 f168=61 ⇔ 真实 0.61%）。
        // 与兄弟 percent 字段 f170(涨跌幅) 一致，归一化到 shared `Percent`（百分点）。
        turnover_rate: d.f168.map(|v| v / 100.0),
        volume_ratio: None,
        limit_up: None,
        limit_down: None,
        bid,
        ask,
        trade_status: TradeStatus::Unknown,
        source: QuoteSource::Eastmoney,
        captured_at: now,
        exchange_time,
        // provider 只填 source / capturedAt；status / warning 由 query facade 派生（spec §5）。
        freshness: Freshness {
            status: FreshnessStatus::Fresh,
            captured_at: Some(now),
            exchange_time,
            age_ms: Some(0),
            source: Some("eastmoney".to_string()),
            warning: None,
        },
        warnings: Vec::new(),
    })
}

pub(crate) fn em_div100(v: f64) -> Option<Price> {
    if !v.is_finite() || v <= 0.0 {
        return None;
    }
    Decimal::from_f64(v / 100.0).map(|d| Price(d.round_dp(4)))
}

/// EM secid 映射。
///
/// 市场前缀：SH=1，SZ=0，BJ=0。
///
/// BJ 前缀证据（Spec finding #2）：EM push2 对北交所使用 market=0 前缀（与 SZ 同），
/// `2.<code>` 实测无数据。审计在可用环境（2026-05-30）下用 `0.<code>` 拿到了 payload。
/// 故保留 BJ → `0.`；对 0 价/占位 payload 由 fetch_quote 的 completeness guard 拦截，
/// 不会作为可用行情发出。注意：此前缀无法在当前沙箱内复测（EM /api/qt/* 出口被屏蔽，
/// HTTP 层无响应），如未来 EM 改规则需重新 live 验证 920 系新代码。
pub(crate) fn secid_of(ts_code: &TsCode) -> String {
    let market = match ts_code.market() {
        crate::domain::shared::Market::SH => "1",
        crate::domain::shared::Market::SZ => "0",
        crate::domain::shared::Market::BJ => "0",
    };
    format!("{}.{}", market, &ts_code.as_str()[..6])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn em_div100_divides_by_100_and_rounds() {
        let p = em_div100(150000.0).unwrap();
        assert_eq!(p.0.to_string(), "1500");
    }

    #[test]
    fn em_div100_rejects_zero_and_negative() {
        assert!(em_div100(0.0).is_none());
        assert!(em_div100(-1.0).is_none());
        assert!(em_div100(f64::NAN).is_none());
    }

    #[test]
    fn em_secid_sh_maps_to_1_prefix() {
        let c = TsCode::parse("600519.SH").unwrap();
        assert_eq!(secid_of(&c), "1.600519");
    }

    #[test]
    fn em_secid_sz_maps_to_0_prefix() {
        let c = TsCode::parse("000001.SZ").unwrap();
        assert_eq!(secid_of(&c), "0.000001");
    }

    #[test]
    fn em_secid_bj_maps_to_0_prefix() {
        let c = TsCode::parse("430047.BJ").unwrap();
        assert_eq!(secid_of(&c), "0.430047");
    }

    fn test_ctx() -> (TsCode, InstrumentCategory, TradeDate, OccurredAt) {
        let ts = TsCode::parse("600519.SH").unwrap();
        let td = TradeDate::from_naive(NaiveDate::from_ymd_opt(2026, 5, 29).unwrap());
        let now = Utc.with_ymd_and_hms(2026, 5, 29, 6, 0, 0).single().unwrap();
        (ts, InstrumentCategory::Stock, td, now)
    }

    // FIX A: EM f168 是换手率×100；归一化后 600519 f168=61 → turnover_rate == 0.61。
    #[test]
    fn em_quote_turnover_rate_divided_by_100() {
        let json = r#"{"data":{
            "f43":170000,"f57":"600519","f58":"贵州茅台",
            "f60":169000,"f168":61,"f170":59
        }}"#;
        let resp: QuoteResp = serde_json::from_str(json).unwrap();
        let (ts, cat, td, now) = test_ctx();
        let q = parse_quote(resp.data.unwrap(), &ts, cat, td, now).unwrap();
        // f168=61 → 0.61% 换手率（shared Percent 百分点约定）。
        assert_eq!(q.turnover_rate, Some(0.61));
        // sibling f170=59 → 0.59% 涨跌幅，确认同一约定。
        assert_eq!(q.change_percent, Some(0.59));
        // price f43=170000 → 1700.0。
        assert_eq!(q.price.unwrap().0.to_string(), "1700");
    }

    // FIX C: 占位/0 价 payload（name 含「已切换」）不应作为可用行情发出。
    #[test]
    fn em_quote_rejects_zero_price_placeholder() {
        let json = r#"{"data":{"f43":0,"f57":"430047","f58":"XX已切换至新代码","f60":0}}"#;
        let resp: QuoteResp = serde_json::from_str(json).unwrap();
        let (ts, cat, td, now) = test_ctx();
        let r = parse_quote(resp.data.unwrap(), &ts, cat, td, now);
        assert!(matches!(r, Err(EmError::Empty)));
    }

    // FIX B: diff 为对象（数字键 map）形态应被解析。
    #[test]
    fn em_bj_universe_parses_object_diff() {
        let json = r#"{"data":{"diff":{
            "0":{"f12":"810011","f14":"优机定转"},
            "1":{"f12":"810013","f14":"万通定转"}
        }}}"#;
        let out = parse_bj_universe(json).unwrap();
        assert_eq!(
            out,
            vec![
                ("810011".to_string(), "优机定转".to_string()),
                ("810013".to_string(), "万通定转".to_string()),
            ]
        );
    }

    // FIX B: diff 为数组形态仍应被解析（向后兼容）。
    #[test]
    fn em_bj_universe_parses_array_diff() {
        let json = r#"{"data":{"diff":[
            {"f12":"430047","f14":"诺思兰德"},
            {"f12":"920019","f14":"某新代码"}
        ]}}"#;
        let out = parse_bj_universe(json).unwrap();
        assert_eq!(
            out,
            vec![
                ("430047".to_string(), "诺思兰德".to_string()),
                ("920019".to_string(), "某新代码".to_string()),
            ]
        );
    }

    // FIX B: diff 缺失 / null 时返回空列表，不报错。
    #[test]
    fn em_bj_universe_tolerates_missing_diff() {
        assert!(parse_bj_universe(r#"{"data":{}}"#).unwrap().is_empty());
        assert!(parse_bj_universe(r#"{"data":{"diff":null}}"#)
            .unwrap()
            .is_empty());
    }
}
