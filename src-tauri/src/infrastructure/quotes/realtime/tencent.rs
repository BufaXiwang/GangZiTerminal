//! 腾讯财经 `qt.gtimg.cn` 实时报价适配——走 proxy_pool。
//!
//! 接口：`GET http://qt.gtimg.cn/q=sh600519,sz000001,bj920469`
//! 响应（GBK 编码）：
//! ```
//! v_sh600519="1~贵州茅台~600519~1789.000~1788.000~1789.000~..."; v_sz000001="...";
//! ```
//!
//! 字段位置（经典 40+ 字段格式）：
//! - [1] name / [2] code
//! - [3] price / [4] prev_close / [5] open
//! - [6] 成交量(手) — 我们用 [36] 重复字段，更稳
//! - [30] time YYYYMMDDHHmmss
//! - [31] 涨跌(元) / [32] 涨跌幅(%)
//! - [33] high / [34] low
//! - [36] 成交量(手) / [37] 成交额(万元 → ×10000 = 元)
//!
//! batch 上限：60（easyquotation 实测）。

use super::client_cache::ProxyClientCache;
use super::proxy_pool::pool;
use super::{split_ts_code, RealtimeQuoteSource};
use crate::domain::quotes::{QuotesError, StockQuote};
use crate::domain::shared::{Lots, OccurredAt, StockCode, Yuan};
use async_trait::async_trait;
use std::sync::OnceLock;
use std::time::Duration;

const URL_BASE: &str = "http://qt.gtimg.cn/q=";
const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";
const TIMEOUT: Duration = Duration::from_secs(10);

static CACHE: OnceLock<ProxyClientCache> = OnceLock::new();

fn cache() -> &'static ProxyClientCache {
    CACHE.get_or_init(|| {
        ProxyClientCache::new(|| reqwest::Client::builder().timeout(TIMEOUT).user_agent(UA))
    })
}

pub struct TencentSource;

impl TencentSource {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl RealtimeQuoteSource for TencentSource {
    fn name(&self) -> &'static str {
        "tencent"
    }

    fn batch_limit(&self) -> usize {
        60
    }

    async fn fetch(&self, ts_codes: &[String]) -> Result<Vec<(String, StockQuote)>, QuotesError> {
        if ts_codes.is_empty() {
            return Ok(Vec::new());
        }

        let tencent_codes: Vec<String> = ts_codes
            .iter()
            .filter_map(|ts| {
                let (prefix, code) = split_ts_code(ts)?;
                Some(format!("{prefix}{code}"))
            })
            .collect();
        if tencent_codes.is_empty() {
            return Ok(Vec::new());
        }

        let url = format!("{URL_BASE}{}", tencent_codes.join(","));

        let attempts = pool().ordered_attempts();
        if attempts.is_empty() {
            return Err(QuotesError::Network("tencent: 无可用 proxy".into()));
        }

        let mut last_err: Option<QuotesError> = None;
        for (proxy_url, idx) in attempts {
            let client = match cache().get(proxy_url.as_deref()) {
                Ok(c) => c,
                Err(e) => {
                    last_err = Some(e);
                    pool().report(idx, false);
                    continue;
                }
            };
            match do_request(&client, &url).await {
                Ok(items) => {
                    pool().report(idx, true);
                    return Ok(items);
                }
                Err(e) => {
                    tracing::debug!(proxy = ?proxy_url, err = %e, "tencent proxy attempt failed");
                    pool().report(idx, false);
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| QuotesError::Network("tencent: 所有 proxy 都失败".into())))
    }
}

async fn do_request(
    client: &reqwest::Client,
    url: &str,
) -> Result<Vec<(String, StockQuote)>, QuotesError> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| QuotesError::Network(format!("腾讯 请求失败: {e}")))?;
    if !resp.status().is_success() {
        return Err(QuotesError::Network(format!("腾讯 HTTP {}", resp.status())));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| QuotesError::Network(format!("腾讯 读取失败: {e}")))?;
    let (text, _, _) = encoding_rs::GBK.decode(&bytes);
    parse_response(&text)
}

fn parse_response(body: &str) -> Result<Vec<(String, StockQuote)>, QuotesError> {
    let mut result: Vec<(String, StockQuote)> = Vec::new();

    for line in body.split(';') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // v_sh600519="1~..."
        let eq = match line.find('=') {
            Some(i) => i,
            None => continue,
        };
        let key = line[..eq].trim();
        let value = line[eq + 1..].trim().trim_matches('"');
        if value.is_empty() {
            continue;
        }
        if !key.starts_with("v_") {
            continue;
        }
        let tcode = &key[2..]; // "sh600519"
        if tcode.len() < 8 {
            continue;
        }

        let fields: Vec<&str> = value.split('~').collect();
        if fields.len() < 35 {
            continue;
        }

        let suffix = match &tcode[..2] {
            "sh" => "SH",
            "sz" => "SZ",
            "bj" => "BJ",
            _ => continue,
        };

        let code_str = fields[2].trim();
        let code = match StockCode::new(code_str) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let name = fields[1].trim().to_string();
        let ts_code = format!("{code_str}.{suffix}");

        let parse_f = |i: usize| -> Option<f64> {
            fields
                .get(i)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse::<f64>().ok())
        };

        let price = parse_f(3).map(Yuan::from_unchecked);
        let prev_close = parse_f(4).map(Yuan::from_unchecked);
        let open = parse_f(5).map(Yuan::from_unchecked);
        let change = parse_f(31).map(Yuan::from_unchecked);
        let change_percent = parse_f(32);
        let high = parse_f(33).map(Yuan::from_unchecked);
        let low = parse_f(34).map(Yuan::from_unchecked);
        let day_volume = parse_f(36)
            .or_else(|| parse_f(6))
            .map(|v| Lots::from_unchecked(v as i64));
        let day_amount = parse_f(37).map(|v| Yuan::from_unchecked(v * 10000.0));

        result.push((
            ts_code,
            StockQuote {
                code,
                name,
                price,
                change_percent,
                change,
                open,
                high,
                low,
                previous_close: prev_close,
                day_volume,
                day_amount,
                captured_at: OccurredAt::now(),
                bid_levels: Vec::new(),
                ask_levels: Vec::new(),
                buy_volume: None,
                sell_volume: None,
                order_imbalance: None,
            },
        ));
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_stock() {
        let body = "v_sh600519=\"1~贵州茅台~600519~1789.000~1788.000~1789.500~3856~1929~1927~~~~~~~~~~~~~~~~~~~~~~~~20260514150003~1.000~0.06~1790.000~1786.000~~3856~6906120~~~~\";";
        let r = parse_response(body).unwrap();
        assert_eq!(r.len(), 1);
        let (ts, q) = &r[0];
        assert_eq!(ts, "600519.SH");
        assert_eq!(q.name, "贵州茅台");
        assert_eq!(q.code.as_str(), "600519");
        assert!((q.price.as_ref().unwrap().value() - 1789.000).abs() < 1e-6);
        assert!((q.previous_close.as_ref().unwrap().value() - 1788.000).abs() < 1e-6);
    }

    #[test]
    fn parse_empty_quote_skipped() {
        let body = "v_sh999999=\"\";";
        let r = parse_response(body).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn parse_handles_bj() {
        let body = "v_bj920469=\"2~富恒新材~920469~12.34~12.10~12.20~1000~~~~~~~~~~~~~~~~~~~~~~~~~~~~20260514150003~0.24~1.98~12.50~12.00~~1000~123400~~~~\";";
        let r = parse_response(body).unwrap();
        assert_eq!(r.len(), 1);
        let (ts, q) = &r[0];
        assert_eq!(ts, "920469.BJ");
        assert_eq!(q.code.as_str(), "920469");
    }
}
