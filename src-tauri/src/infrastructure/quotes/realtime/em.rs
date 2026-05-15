//! 东方财富 `ulist.np` 实时报价适配——走 proxy_pool。
//!
//! 解析逻辑沿用 `infrastructure::quotes::eastmoney::realtime::*_with`，
//! 这里只负责：ts_code → EM secid 转换 + 通过 proxy_pool 轮换 client。

use super::client_cache::ProxyClientCache;
use super::proxy_pool::pool;
use super::{split_ts_code, RealtimeQuoteSource};
use crate::domain::quotes::{QuotesError, StockQuote};
use crate::infrastructure::quotes::eastmoney::realtime as em_inner;
use async_trait::async_trait;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, REFERER};
use std::sync::OnceLock;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(12);
const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

static CACHE: OnceLock<ProxyClientCache> = OnceLock::new();

fn cache() -> &'static ProxyClientCache {
    CACHE.get_or_init(|| {
        ProxyClientCache::new(|| {
            reqwest::Client::builder()
                .timeout(TIMEOUT)
                .user_agent(UA)
                .default_headers(
                    [
                        (REFERER, "https://quote.eastmoney.com/".parse().unwrap()),
                        (ACCEPT, "application/json,text/plain,*/*".parse().unwrap()),
                        (ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.8".parse().unwrap()),
                    ]
                    .into_iter()
                    .collect(),
                )
        })
    })
}

pub struct EmSource;

impl EmSource {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl RealtimeQuoteSource for EmSource {
    fn name(&self) -> &'static str {
        "em"
    }

    fn batch_limit(&self) -> usize {
        300
    }

    async fn fetch(&self, ts_codes: &[String]) -> Result<Vec<(String, StockQuote)>, QuotesError> {
        let secids: Vec<String> = ts_codes
            .iter()
            .filter_map(|ts| {
                let (prefix, code) = split_ts_code(ts)?;
                let m = match prefix {
                    "sh" => "1",
                    "sz" => "0",
                    "bj" => "2",
                    _ => return None,
                };
                Some(format!("{m}.{code}"))
            })
            .collect();
        if secids.is_empty() {
            return Ok(Vec::new());
        }

        let attempts = pool().ordered_attempts();
        if attempts.is_empty() {
            return Err(QuotesError::Network("em: 无可用 proxy".into()));
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
            match em_inner::fetch_quotes_by_secids_with(&client, &secids).await {
                Ok(items) => {
                    pool().report(idx, true);
                    return Ok(items);
                }
                Err(e) => {
                    tracing::debug!(proxy = ?proxy_url, err = %e, "em proxy attempt failed");
                    pool().report(idx, false);
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| QuotesError::Network("em: 所有 proxy 都失败".into())))
    }
}
