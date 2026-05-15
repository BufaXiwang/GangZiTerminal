//! Eastmoney HTTP client——通用 GET + 重试 + 错误转换。
//!
//! EM 接口大部分是 GET，response 是带 JSONP 或纯 JSON。共用 client + retry 策略：
//! - push2.eastmoney.com：实时行情，稳定
//! - push2his.eastmoney.com：历史 K 线 / 分时，偶发 TLS 风控

use crate::domain::quotes::QuotesError;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, REFERER};
use serde_json::Value;
use std::sync::OnceLock;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(12);
const RETRY_COUNT: usize = 3;
const RETRY_BASE_MS: u64 = 300;

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn http_client() -> Result<&'static reqwest::Client, QuotesError> {
    if let Some(c) = HTTP_CLIENT.get() {
        return Ok(c);
    }
    let c = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36")
        .default_headers(
            [
                (REFERER, "https://quote.eastmoney.com/".parse().unwrap()),
                (ACCEPT, "application/json,text/plain,*/*".parse().unwrap()),
                (ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.8".parse().unwrap()),
            ]
            .into_iter()
            .collect(),
        )
        .build()
        .map_err(|e| QuotesError::Network(e.to_string()))?;
    Ok(HTTP_CLIENT.get_or_init(|| c))
}

/// GET 一次性请求——不重试。push2.eastmoney.com 实时接口用。
pub async fn fetch_text(url: &str, label: &str) -> Result<String, QuotesError> {
    let client = http_client()?;
    fetch_text_with(client, url, label).await
}

/// 同 `fetch_text`，但允许调用方传入自定义 Client（含 proxy）。
pub async fn fetch_text_with(
    client: &reqwest::Client,
    url: &str,
    label: &str,
) -> Result<String, QuotesError> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| QuotesError::Network(format!("{label} 请求失败：{e}")))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| QuotesError::Network(format!("{label} 响应读取失败：{e}")))?;
    if !status.is_success() {
        return Err(QuotesError::Network(format!("{label} HTTP {status}")));
    }
    if body.trim().is_empty() {
        return Err(QuotesError::Network(format!("{label} 响应为空")));
    }
    Ok(body)
}

/// GET + 重试——push2his.eastmoney.com K 线 / 分时接口用，偶发 Empty reply。
pub async fn fetch_text_with_retry(url: &str, label: &str) -> Result<String, QuotesError> {
    let client = http_client()?;
    let mut last_err: Option<QuotesError> = None;
    for attempt in 0..RETRY_COUNT {
        match client.get(url).send().await {
            Ok(resp) => match resp.text().await {
                Ok(body) if !body.trim().is_empty() => return Ok(body),
                Ok(_) => {
                    last_err = Some(QuotesError::Network(format!("{label} 响应为空")));
                }
                Err(err) => {
                    last_err = Some(QuotesError::Network(format!("{label} 响应读取失败：{err}")));
                }
            },
            Err(err) => {
                last_err = Some(QuotesError::Network(format!("{label} 请求失败：{err}")));
            }
        }
        if attempt < RETRY_COUNT - 1 {
            tokio::time::sleep(Duration::from_millis(RETRY_BASE_MS * (attempt as u64 + 1))).await;
        }
    }
    Err(last_err.unwrap_or_else(|| QuotesError::Network(format!("{label} 请求失败"))))
}

/// 解 EM JSON 响应——业务字段在 `data.diff` 或 `data.trends` 数组里。
pub fn parse_em_response(body: &str, label: &str) -> Result<Value, QuotesError> {
    serde_json::from_str(body)
        .map_err(|e| QuotesError::Decode(format!("{label} JSON 解析失败：{e}")))
}
