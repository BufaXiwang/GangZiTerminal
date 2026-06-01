//! Tencent quote adapter — 实时行情低优先级 fallback。
//!
//! Spec: docs/design/quotes-module.md §5；docs/design/references/quotes/tencent.md

use crate::domain::quotes::{QuoteDepthLevel, QuoteSource, StockQuote, TradeStatus};
use crate::domain::shared::{
    Amount, Freshness, FreshnessStatus, InstrumentCategory, Market, OccurredAt, Price, TradeDate,
    TsCode, Volume,
};
use chrono::{NaiveDateTime, TimeZone, Utc};
use chrono_tz::Asia::Shanghai;
use encoding_rs::GBK;
use reqwest::Client;
use rust_decimal::{prelude::FromPrimitive, Decimal};
use std::time::Duration;
use thiserror::Error;

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum TencentError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("provider returned no data")]
    Empty,
}

#[derive(Clone)]
pub struct TencentProvider {
    client: Client,
}

impl TencentProvider {
    pub fn new() -> reqwest::Result<Self> {
        let client = Client::builder().timeout(TIMEOUT).build()?;
        Ok(Self { client })
    }

    /// 拉单只标的实时行情。HTTP + GBK 解码 + 提取引号内 payload，解析逻辑委托
    /// [`parse_tencent_payload`]（纯函数，可单测）。
    pub async fn fetch_quote(
        &self,
        ts_code: &TsCode,
        category: InstrumentCategory,
        eligible_trade_date: TradeDate,
        now: OccurredAt,
    ) -> Result<StockQuote, TencentError> {
        let qid = qq_id(ts_code);
        let url = format!("https://qt.gtimg.cn/q={}", qid);
        let bytes = self.client.get(&url).send().await?.bytes().await?;
        let (cow, _enc, _had_err) = GBK.decode(&bytes);
        let body = cow.into_owned();
        // 格式：v_sh600519="1~贵州茅台~600519~..."
        let payload = body
            .split('"')
            .nth(1)
            .ok_or(TencentError::Empty)?;
        if payload.is_empty() {
            return Err(TencentError::Empty);
        }
        parse_tencent_payload(payload, ts_code, category, eligible_trade_date, now)
    }
}

/// 纯函数：解析腾讯 `v_xxx="..."` 引号内的 `~` 分隔 payload → domain `StockQuote`。
///
/// 字段映射 / 单位换算与原 `fetch_quote` 内联逻辑完全一致（仅抽函数，不改逻辑）：
/// volume / 盘口量单位「手」×100 转股；amount 单位「万元」×10000 转元；
/// price/盘口价为真值不缩放（腾讯无 EM 的 10^f59 缩放问题）。
///
/// Spec: docs/design/quotes-module.md §5；docs/design/references/quotes/tencent.md
pub(crate) fn parse_tencent_payload(
    payload: &str,
    ts_code: &TsCode,
    category: InstrumentCategory,
    eligible_trade_date: TradeDate,
    now: OccurredAt,
) -> Result<StockQuote, TencentError> {
    let parts: Vec<&str> = payload.split('~').collect();
    // Guard only ensures the directly-indexed fields (name[1], price[3], prev[4], open[5],
    // volume[6]) exist; everything richer (五档/time/high/low/amount/turnover) uses safe `.get()`
    // and is optional. BJ pre-open snapshots come back with only ~41 fields (vs ~62 for SH/SZ) —
    // the old `< 50` guard wrongly rejected BJ even though Tencent is now its main quote path
    // (spec §5). Field-richness / freshness is judged downstream by is_display_complete /
    // is_quote_complete; here we only reject clearly-empty/malformed responses.
    if parts.len() < 10 {
        return Err(TencentError::Parse(format!(
            "too few fields: {}",
            parts.len()
        )));
    }
    let parse_p = |s: &str| -> Option<Price> {
            s.parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v > 0.0)
                .and_then(|v| Decimal::from_f64(v).map(|d| Price(d.round_dp(4))))
        };
        let parse_v = |s: &str| -> Option<Volume> {
            // QQ volume 单位是手（百股）。
            s.parse::<f64>()
                .ok()
                .filter(|v| *v > 0.0)
                .map(|v| Volume((v * 100.0) as i64))
        };
        let name = Some(parts[1].to_string());
        let price = parse_p(parts[3]);
        let prev = parse_p(parts[4]);
        let open = parse_p(parts[5]);
        let volume = parts[6]
            .parse::<f64>()
            .ok()
            .filter(|v| *v > 0.0)
            .map(|v| Volume((v * 100.0) as i64));
        // 五档：买一价 [9]，买一量 [10]（手），买二[11/12]... 卖一[19]...
        let mut bid: Vec<QuoteDepthLevel> = Vec::with_capacity(5);
        let mut ask: Vec<QuoteDepthLevel> = Vec::with_capacity(5);
        for i in 0..5 {
            let bp = parse_p(parts.get(9 + i * 2).copied().unwrap_or(""));
            let bv = parse_v(parts.get(10 + i * 2).copied().unwrap_or(""));
            bid.push(QuoteDepthLevel { price: bp, volume: bv });
            let ap = parse_p(parts.get(19 + i * 2).copied().unwrap_or(""));
            let av = parse_v(parts.get(20 + i * 2).copied().unwrap_or(""));
            ask.push(QuoteDepthLevel { price: ap, volume: av });
        }
        // 时间字段 [30]: YYYYMMDDHHMMSS
        let exchange_time = parts
            .get(30)
            .and_then(|s| NaiveDateTime::parse_from_str(s, "%Y%m%d%H%M%S").ok())
            .and_then(|dt| Shanghai.from_local_datetime(&dt).single())
            .map(|t| t.with_timezone(&Utc));
        let high = parts.get(33).and_then(|s| parse_p(s));
        let low = parts.get(34).and_then(|s| parse_p(s));
        let amount = parts
            .get(37)
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|v| *v > 0.0)
            // QQ amount 单位是万元；× 10000。
            .and_then(|v| Decimal::from_f64(v * 10000.0).map(|d| Amount(d.round_dp(4))));
        let turnover_rate = parts.get(38).and_then(|s| s.parse::<f64>().ok());
        let change = match (price, prev) {
            (Some(p), Some(pc)) => Some(Price(p.0 - pc.0)),
            _ => None,
        };
        let change_percent = match (price, prev) {
            (Some(p), Some(pc)) if pc.0 > Decimal::ZERO => {
                ((p.0 - pc.0) / pc.0 * Decimal::from(100))
                    .round_dp(4)
                    .to_string()
                    .parse::<f64>()
                    .ok()
            }
            _ => None,
        };

        Ok(StockQuote {
            ts_code: ts_code.clone(),
            name,
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
            turnover_rate,
            volume_ratio: None,
            limit_up: None,
            limit_down: None,
            bid,
            ask,
            trade_status: TradeStatus::Unknown,
            source: QuoteSource::Tencent,
            captured_at: now,
            exchange_time,
            freshness: Freshness {
                status: FreshnessStatus::Fresh,
                captured_at: Some(now),
                exchange_time,
                age_ms: Some(0),
                source: Some("tencent".to_string()),
                warning: None,
            },
            warnings: Vec::new(),
        })
}

pub(crate) fn qq_id(ts_code: &TsCode) -> String {
    let prefix = match ts_code.market() {
        Market::SH => "sh",
        Market::SZ => "sz",
        Market::BJ => "bj",
    };
    format!("{}{}", prefix, &ts_code.as_str()[..6])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qq_id_sh_prefix() {
        let c = TsCode::parse("600519.SH").unwrap();
        assert_eq!(qq_id(&c), "sh600519");
    }

    #[test]
    fn qq_id_sz_prefix() {
        let c = TsCode::parse("000001.SZ").unwrap();
        assert_eq!(qq_id(&c), "sz000001");
    }

    #[test]
    fn qq_id_bj_prefix() {
        let c = TsCode::parse("430047.BJ").unwrap();
        assert_eq!(qq_id(&c), "bj430047");
    }

    // ============================================================== golden parse
    //
    // 腾讯是唯一 HTTP 报价源（spec §5），用 2026-06-01 实测真实 payload 锁死字段映射。
    // payload 为 `v_xxx="..."` 引号内、`~` 分隔的内容。

    use chrono::NaiveDate;

    fn ctx(
        code: &str,
        cat: InstrumentCategory,
    ) -> (TsCode, InstrumentCategory, TradeDate, OccurredAt) {
        let ts = TsCode::parse(code).unwrap();
        let td = TradeDate::from_naive(NaiveDate::from_ymd_opt(2026, 6, 1).unwrap());
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 8, 0, 0).single().unwrap();
        (ts, cat, td, now)
    }

    // 2026-06-01 16:14:02 上海时区 = 08:14:02 UTC。
    fn sh_dt(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> chrono::DateTime<Utc> {
        Shanghai
            .from_local_datetime(&NaiveDate::from_ymd_opt(y, mo, d).unwrap().and_hms_opt(h, mi, s).unwrap())
            .single()
            .unwrap()
            .with_timezone(&Utc)
    }

    const SH600519: &str = "1~贵州茅台~600519~1309.60~1326.00~1327.00~43845~19458~24387~1309.60~59~1309.56~1~1309.50~1~1309.42~1~1309.40~2~1309.99~1~1310.00~4~1310.40~2~1310.50~6~1310.52~1~~20260601161402~-16.40~-1.24~1327.00~1301.31~1309.60/43845/5741133268~43845~574113~0.35~19.79~~1327.00~1301.31~1.94~16371.07~16371.07~6.11~1458.60~1193.40~0.74~50~1309.43~15.02~19.89~~~0.34~574113.3268~0.0000~0~ ~GP-A";
    const SH000001: &str = "1~上证指数~000001~4057.74~4068.57~4067.16~676025806~0~0~0.00~0~0.00~0~0.00~0~0.00~0~0.00~0~0.00~0~0.00~0~0.00~0~0.00~0~0.00~0~~20260601161415~-10.83~-0.27~4093.04~4045.69~4057.74/676025806/1319801913951~676025806~131980191~1.40~17.90~~4093.04~4045.69~1.16~627418.60~675595.49~0.00~-1~-1~1.03~0~4067.73~~~~~~131980191.3951~0.0000~0~ ~ZS";
    const SH510300: &str = "1~沪深300ETF华泰柏瑞~510300~4.868~4.923~4.923~4990927~2384203~2606724~4.868~665~4.867~1931~4.866~6547~4.865~5885~4.864~899~4.869~532~4.870~1039~4.871~1953~4.872~2489~4.873~927~~20260601161448~-0.055~-1.12~4.940~4.861~4.868/4990927/2443095510~4990927~244310~1.79~~~4.940~4.861~1.60~1360.91~1360.91~0.00~5.415~4.431~0.58~8987~4.895~~~~~~244309.5510~0.0000~0~ ~ETF";
    const BJ430047: &str = "62~诺思兰德~430047~8.17~8.17~0.00~0~0~0~0.00~0~0.00~0~0.00~0~0.00~0~0.00~0~0.00~0~0.00~0~0.00~0~0.00~0~0.00~0~~20260601090000~0.00~0.00~0.00~0.00~8.17/0/0~0~0.00~0.00~-38.71~C";

    #[test]
    fn tencent_golden_stock_sh600519() {
        let (ts, cat, td, now) = ctx("600519.SH", InstrumentCategory::Stock);
        let q = parse_tencent_payload(SH600519, &ts, cat, td, now).unwrap();
        // 注：rust_decimal `from_f64` 归一化尾随零，故 1309.60 → "1309.6"、1326.00 → "1326"。
        assert_eq!(q.name.as_deref(), Some("贵州茅台"));
        assert_eq!(q.price.unwrap().0.to_string(), "1309.6");
        assert_eq!(q.previous_close.unwrap().0.to_string(), "1326");
        assert_eq!(q.open.unwrap().0.to_string(), "1327");
        assert_eq!(q.high.unwrap().0.to_string(), "1327");
        assert_eq!(q.low.unwrap().0.to_string(), "1301.31");
        // 43845 手 × 100 = 4384500 股。
        assert_eq!(q.volume.unwrap().0, 4_384_500);
        // 574113 万元 × 10000 = 5_741_130_000 元。
        assert_eq!(q.amount.unwrap().0.to_string(), "5741130000");
        assert_eq!(q.turnover_rate, Some(0.35));
        // 买一 1309.60 / 59 手 → 5900 股。
        assert_eq!(q.bid[0].price.unwrap().0.to_string(), "1309.6");
        assert_eq!(q.bid[0].volume.unwrap().0, 5_900);
        // 卖一 1309.99。
        assert_eq!(q.ask[0].price.unwrap().0.to_string(), "1309.99");
        // exchange_time = 2026-06-01 16:14:02 上海时区。
        assert_eq!(q.exchange_time, Some(sh_dt(2026, 6, 1, 16, 14, 2)));
        // change / change_percent 由 price - prev 算出（≈ -16.40 / ≈ -1.24%）。
        assert_eq!(q.change.unwrap().0.to_string(), "-16.4");
        assert_eq!(q.change_percent, Some(-1.2368));
    }

    #[test]
    fn tencent_golden_index_sh000001() {
        let (ts, cat, td, now) = ctx("000001.SH", InstrumentCategory::Index);
        let q = parse_tencent_payload(SH000001, &ts, cat, td, now).unwrap();
        assert_eq!(q.price.unwrap().0.to_string(), "4057.74");
        // 五档买卖价全 0 → parse_p 过滤 ≤0 → 全 None。
        for lvl in q.bid.iter().chain(q.ask.iter()) {
            assert!(lvl.price.is_none());
        }
        assert_eq!(q.turnover_rate, Some(1.40));
    }

    #[test]
    fn tencent_golden_etf_sh510300_three_decimals_preserved() {
        let (ts, cat, td, now) = ctx("510300.SH", InstrumentCategory::Fund);
        let q = parse_tencent_payload(SH510300, &ts, cat, td, now).unwrap();
        // 3 位小数为真值，不缩放 / 不被破坏。
        assert_eq!(q.price.unwrap().0.to_string(), "4.868");
        assert_eq!(q.previous_close.unwrap().0.to_string(), "4.923");
        assert_eq!(q.bid[0].price.unwrap().0.to_string(), "4.868");
    }

    // BJ 盘前快照 payload 只有 41 字段（SH/SZ ~62）。腾讯是 BJ 实时报价主路径（spec §5），
    // 守卫已从 `< 50` 放宽到 `< 10`，故 BJ 现在能正确解出 price=8.17（盘前 open/high/low/vol 为 0
    // → None，下游 is_display_complete 用 price 非空判定可用）。
    #[test]
    fn tencent_golden_bj430047_short_payload_parses_price() {
        let (ts, cat, td, now) = ctx("430047.BJ", InstrumentCategory::Stock);
        assert_eq!(BJ430047.split('~').count(), 41);
        let q = parse_tencent_payload(BJ430047, &ts, cat, td, now).unwrap();
        assert_eq!(q.price.unwrap().0.to_string(), "8.17");
        assert_eq!(q.previous_close.unwrap().0.to_string(), "8.17");
        // 盘前：开/高/低/量为 0 → None。
        assert!(q.open.is_none());
        assert!(q.high.is_none());
        assert!(q.low.is_none());
        assert!(q.volume.is_none());
        // exchange_time 对应 2026-06-01 09:00:00 上海时区。
        assert!(q.exchange_time.is_some());
    }
}
