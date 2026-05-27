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

    /// 拉单只标的实时行情。
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
            .ok_or(TencentError::Empty)?
            .to_string();
        if payload.is_empty() {
            return Err(TencentError::Empty);
        }
        let parts: Vec<&str> = payload.split('~').collect();
        if parts.len() < 50 {
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
}

fn qq_id(ts_code: &TsCode) -> String {
    let prefix = match ts_code.market() {
        Market::SH => "sh",
        Market::SZ => "sz",
        Market::BJ => "bj",
    };
    format!("{}{}", prefix, &ts_code.as_str()[..6])
}
