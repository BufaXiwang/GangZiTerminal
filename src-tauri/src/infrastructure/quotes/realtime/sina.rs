//! 新浪财经 `hq.sinajs.cn` 实时报价适配——走 proxy_pool。
//!
//! 接口：`GET http://hq.sinajs.cn/list=sh600519,sz000001`
//! Header：`Referer: http://finance.sina.com.cn`（否则 403）
//! 响应（GBK 编码）：
//! ```
//! var hq_str_sh600519="贵州茅台,1789.000,1788.000,1789.990,1791.000,1786.000,...";
//! ```
//!
//! 字段位置（30+ 字段）：
//! - [0] name
//! - [1] open / [2] prev_close / [3] price
//! - [4] high / [5] low
//! - [8] 成交量（**股** → ÷100 = 手）
//! - [9] 成交额（**元**）
//! - [30] date YYYY-MM-DD / [31] time HH:MM:SS
//!
//! 新浪不直接给 change / change_pct，需要派生：
//! - change = price - prev_close
//! - change_pct = change / prev_close * 100
//!
//! batch 上限：800（easyquotation 实测）；北交所支持不完整，作为兜底。

use super::client_cache::ProxyClientCache;
use super::proxy_pool::pool;
use super::{split_ts_code, RealtimeQuoteSource};
use crate::domain::quotes::{QuotesError, StockQuote};
use crate::domain::shared::{Lots, OccurredAt, StockCode, Yuan};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, REFERER, USER_AGENT};
use std::sync::OnceLock;
use std::time::Duration;

const URL_BASE: &str = "http://hq.sinajs.cn/list=";
const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";
const REFERER_VAL: &str = "http://finance.sina.com.cn";
const TIMEOUT: Duration = Duration::from_secs(10);

static CACHE: OnceLock<ProxyClientCache> = OnceLock::new();

fn cache() -> &'static ProxyClientCache {
    CACHE.get_or_init(|| {
        ProxyClientCache::new(|| {
            let mut headers = HeaderMap::new();
            headers.insert(REFERER, HeaderValue::from_static(REFERER_VAL));
            headers.insert(USER_AGENT, HeaderValue::from_static(UA));
            reqwest::Client::builder()
                .timeout(TIMEOUT)
                .default_headers(headers)
        })
    })
}

pub struct SinaSource;

impl SinaSource {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl RealtimeQuoteSource for SinaSource {
    fn name(&self) -> &'static str {
        "sina"
    }

    fn batch_limit(&self) -> usize {
        800
    }

    async fn fetch(&self, ts_codes: &[String]) -> Result<Vec<(String, StockQuote)>, QuotesError> {
        if ts_codes.is_empty() {
            return Ok(Vec::new());
        }

        let sina_codes: Vec<String> = ts_codes
            .iter()
            .filter_map(|ts| {
                let (prefix, code) = split_ts_code(ts)?;
                Some(format!("{prefix}{code}"))
            })
            .collect();
        if sina_codes.is_empty() {
            return Ok(Vec::new());
        }

        let url = format!("{URL_BASE}{}", sina_codes.join(","));

        let attempts = pool().ordered_attempts();
        if attempts.is_empty() {
            return Err(QuotesError::Network("sina: 无可用 proxy".into()));
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
                    tracing::debug!(proxy = ?proxy_url, err = %e, "sina proxy attempt failed");
                    pool().report(idx, false);
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| QuotesError::Network("sina: 所有 proxy 都失败".into())))
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
        .map_err(|e| QuotesError::Network(format!("新浪 请求失败: {e}")))?;
    if !resp.status().is_success() {
        return Err(QuotesError::Network(format!("新浪 HTTP {}", resp.status())));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| QuotesError::Network(format!("新浪 读取失败: {e}")))?;
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
        let eq = match line.find('=') {
            Some(i) => i,
            None => continue,
        };
        let key = line[..eq].trim();
        let value = line[eq + 1..].trim().trim_matches('"');
        if value.is_empty() {
            continue;
        }

        let scode_start = match key.rfind('_') {
            Some(i) => i + 1,
            None => continue,
        };
        let scode = &key[scode_start..];
        if scode.len() < 8 {
            continue;
        }

        let fields: Vec<&str> = value.split(',').collect();
        if fields.len() < 10 {
            continue;
        }

        let suffix = match &scode[..2] {
            "sh" => "SH",
            "sz" => "SZ",
            "bj" => "BJ",
            _ => continue,
        };
        let code_str = &scode[2..];
        let code = match StockCode::new(code_str) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let ts_code = format!("{code_str}.{suffix}");

        let parse_f = |i: usize| -> Option<f64> {
            fields
                .get(i)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse::<f64>().ok())
        };

        let name = fields[0].trim().to_string();
        let open = parse_f(1).map(Yuan::from_unchecked);
        let prev_close_raw = parse_f(2);
        let price_raw = parse_f(3);
        let high = parse_f(4).map(Yuan::from_unchecked);
        let low = parse_f(5).map(Yuan::from_unchecked);
        let day_volume = parse_f(8).map(|v| Lots::from_unchecked((v / 100.0) as i64));
        let day_amount = parse_f(9).map(Yuan::from_unchecked);

        let (change, change_percent) = match (price_raw, prev_close_raw) {
            (Some(p), Some(pc)) if pc > 0.0 => {
                let c = p - pc;
                let pct = c / pc * 100.0;
                (Some(Yuan::from_unchecked(c)), Some(pct))
            }
            _ => (None, None),
        };
        let price = price_raw.map(Yuan::from_unchecked);
        let previous_close = prev_close_raw.map(Yuan::from_unchecked);

        if price.is_none() && previous_close.is_none() {
            continue;
        }

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
                previous_close,
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
        let body = r#"var hq_str_sh600519="贵州茅台,1789.000,1788.000,1789.990,1791.000,1786.000,1789.990,1790.000,3856,6906120,3856,1789.990,1929,1789.980,2000,1789.970,1500,1789.960,500,1789.950,1929,1790.000,2000,1790.010,1500,1790.020,500,1790.030,2026-05-14,15:00:00,00";"#;
        let r = parse_response(body).unwrap();
        assert_eq!(r.len(), 1);
        let (ts, q) = &r[0];
        assert_eq!(ts, "600519.SH");
        assert_eq!(q.name, "贵州茅台");
        assert!((q.price.as_ref().unwrap().value() - 1789.990).abs() < 1e-6);
        assert!((q.previous_close.as_ref().unwrap().value() - 1788.000).abs() < 1e-6);
        let c = q.change.as_ref().unwrap().value();
        assert!((c - (1789.990 - 1788.000)).abs() < 1e-6);
        assert_eq!(q.day_volume.as_ref().unwrap().value(), 38);
    }

    #[test]
    fn parse_empty_response_skipped() {
        let body = r#"var hq_str_sh999999="";"#;
        let r = parse_response(body).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn parse_suspended_stock_skipped() {
        let body = r#"var hq_str_sh600000="平安银行,,,,,,,,,,2026-05-14,15:00:00,00";"#;
        let r = parse_response(body).unwrap();
        assert!(r.is_empty());
    }
}
