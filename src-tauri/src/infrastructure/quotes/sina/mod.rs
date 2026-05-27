//! Sina quote adapter — 实时行情最后 fallback（基础展示用）。
//!
//! Spec: docs/design/quotes-module.md §5；docs/design/references/quotes/sina.md
//!
//! 限制：通常不包含可靠盘口；Account 即时成交不得依赖 Sina quote。

use crate::domain::quotes::{QuoteSource, StockQuote, TradeStatus};
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

const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Error)]
pub enum SinaError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("provider returned no data")]
    Empty,
}

#[derive(Clone)]
pub struct SinaProvider {
    client: Client,
}

impl SinaProvider {
    pub fn new() -> reqwest::Result<Self> {
        let client = Client::builder().timeout(TIMEOUT).build()?;
        Ok(Self { client })
    }

    pub async fn fetch_quote(
        &self,
        ts_code: &TsCode,
        category: InstrumentCategory,
        eligible_trade_date: TradeDate,
        now: OccurredAt,
    ) -> Result<StockQuote, SinaError> {
        let sid = sina_id(ts_code);
        let url = format!("https://hq.sinajs.cn/list={}", sid);
        // Sina 要求 Referer 才返回数据。
        let bytes = self
            .client
            .get(&url)
            .header("Referer", "https://finance.sina.com.cn/")
            .send()
            .await?
            .bytes()
            .await?;
        let (cow, _enc, _had_err) = GBK.decode(&bytes);
        let body = cow.into_owned();
        // 格式: var hq_str_<sid>="<name>,open,prev_close,price,high,low,bid1,ask1,volume,amount,b1v,b1p,...,date,time";
        let payload = body
            .split('"')
            .nth(1)
            .ok_or(SinaError::Empty)?
            .to_string();
        if payload.is_empty() {
            return Err(SinaError::Empty);
        }
        let parts: Vec<&str> = payload.split(',').collect();
        if parts.len() < 32 {
            return Err(SinaError::Parse(format!("too few fields: {}", parts.len())));
        }
        let parse_p = |s: &str| -> Option<Price> {
            s.parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v > 0.0)
                .and_then(|v| Decimal::from_f64(v).map(|d| Price(d.round_dp(4))))
        };
        let name = Some(parts[0].to_string());
        let open = parse_p(parts[1]);
        let prev = parse_p(parts[2]);
        let price = parse_p(parts[3]);
        let high = parse_p(parts[4]);
        let low = parse_p(parts[5]);
        let volume = parts[8]
            .parse::<f64>()
            .ok()
            .filter(|v| *v > 0.0)
            .map(|v| Volume(v as i64));
        let amount = parts[9]
            .parse::<f64>()
            .ok()
            .filter(|v| *v > 0.0)
            .and_then(|v| Decimal::from_f64(v).map(|d| Amount(d.round_dp(4))));
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
        // 时间在末两段 date / time
        let date_str = parts[parts.len() - 2];
        let time_str = parts[parts.len() - 1];
        let exchange_time = NaiveDateTime::parse_from_str(
            &format!("{} {}", date_str, time_str),
            "%Y-%m-%d %H:%M:%S",
        )
        .ok()
        .and_then(|dt| Shanghai.from_local_datetime(&dt).single())
        .map(|t| t.with_timezone(&Utc));

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
            turnover_rate: None,
            volume_ratio: None,
            limit_up: None,
            limit_down: None,
            bid: Vec::new(),
            ask: Vec::new(),
            trade_status: TradeStatus::Unknown,
            source: QuoteSource::Sina,
            captured_at: now,
            exchange_time,
            // provider 只填 source / capturedAt；warning 由 query facade 派生（spec §5）。
            freshness: Freshness {
                status: FreshnessStatus::Fresh,
                captured_at: Some(now),
                exchange_time,
                age_ms: Some(0),
                source: Some("sina".to_string()),
                warning: None,
            },
            warnings: Vec::new(),
        })
    }
}

pub(crate) fn sina_id(ts_code: &TsCode) -> String {
    let prefix = match ts_code.market() {
        Market::SH => "sh",
        Market::SZ => "sz",
        // Sina 也支持 bj 标的（"bj430047"）但字段格式可能不同；先尝试。
        Market::BJ => "bj",
    };
    format!("{}{}", prefix, &ts_code.as_str()[..6])
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::GBK;

    #[test]
    fn sina_id_sh_prefix() {
        let c = TsCode::parse("600519.SH").unwrap();
        assert_eq!(sina_id(&c), "sh600519");
    }

    #[test]
    fn sina_id_sz_prefix() {
        let c = TsCode::parse("000001.SZ").unwrap();
        assert_eq!(sina_id(&c), "sz000001");
    }

    #[test]
    fn sina_gbk_roundtrips_chinese_name() {
        // 模拟 Sina 返回 GBK 编码：先编码"贵州茅台"，再用 GBK.decode 解码。
        let original = "贵州茅台";
        let (encoded, _, _) = GBK.encode(original);
        let (decoded, _, _) = GBK.decode(&encoded);
        assert_eq!(decoded, original);
    }
}
