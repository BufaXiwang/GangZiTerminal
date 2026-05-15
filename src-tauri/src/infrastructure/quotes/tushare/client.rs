//! TuShare HTTP client——通用 POST + token + 错误转换。
//!
//! 所有 TuShare 接口共用：单 endpoint <https://api.tushare.pro>，body 里靠 `api_name` 路由。
//! 响应是列式 2D 数组（`{fields:[...], items:[[...]]}`），本模块统一转成行式 `Vec<HashMap>`。
//!
//! Token 从 `app_state[KEY_TUSHARE_TOKEN]` 读——用户在 Settings UI 配。
//! 缺 token → `QuotesError::MissingToken`。

use crate::db;
use crate::domain::quotes::QuotesError;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;
use tauri::AppHandle;

const KEY_TUSHARE_TOKEN: &str = "gangzi-terminal.tushare-token";
const API_ENDPOINT: &str = "https://api.tushare.pro";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// ============================================================================
// HTTP client（全局共享）
// ============================================================================

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn http_client() -> Result<&'static reqwest::Client, QuotesError> {
    if let Some(c) = HTTP_CLIENT.get() {
        return Ok(c);
    }
    let c = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| QuotesError::Network(e.to_string()))?;
    Ok(HTTP_CLIENT.get_or_init(|| c))
}

// ============================================================================
// Token 读取
// ============================================================================

pub(crate) fn read_token(app: &AppHandle) -> Option<String> {
    db::load_app_state_value(app, KEY_TUSHARE_TOKEN)
        .ok()
        .flatten()
        .and_then(|v| v.as_str().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
}

// ============================================================================
// 响应 schema
// ============================================================================

#[derive(Debug, Deserialize)]
struct TushareResponse {
    code: i64,
    msg: Option<String>,
    data: Option<TushareData>,
}

#[derive(Debug, Deserialize)]
struct TushareData {
    fields: Vec<String>,
    items: Vec<Vec<Value>>,
}

// ============================================================================
// 核心：通用 call
// ============================================================================

/// 调任意 TuShare 接口，2D 列式响应转成行式 `Vec<HashMap<字段名, Value>>`。
///
/// 错误分级（QuotesError）：
/// - `MissingToken`：token 没配
/// - `Network`：HTTP / 超时
/// - `Decode`：响应 schema 异常
/// - `RateLimited`：code 40203 / 40211（接口频率限制）
/// - `QuotaExceeded`：code 40202 / 40219（积分不足）
/// - `Provider`：其它业务错误
pub async fn call(
    app: &AppHandle,
    api_name: &str,
    params: Value,
    fields: &str,
) -> Result<Vec<HashMap<String, Value>>, QuotesError> {
    let token = read_token(app).ok_or(QuotesError::MissingToken)?;
    let body = json!({
        "api_name": api_name,
        "token": token,
        "params": params,
        "fields": fields,
    });

    let client = http_client()?;
    let resp = client
        .post(API_ENDPOINT)
        .json(&body)
        .send()
        .await
        .map_err(|e| QuotesError::Network(e.to_string()))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| QuotesError::Network(format!("读响应失败：{e}")))?;

    if !status.is_success() {
        return Err(QuotesError::Network(format!(
            "HTTP {} —— body: {}",
            status,
            text.chars().take(200).collect::<String>()
        )));
    }

    let parsed: TushareResponse = serde_json::from_str(&text).map_err(|e| {
        QuotesError::Decode(format!(
            "{e} —— body: {}",
            text.chars().take(200).collect::<String>()
        ))
    })?;

    if parsed.code != 0 {
        return Err(map_business_error(parsed.code, parsed.msg));
    }

    let data = parsed
        .data
        .ok_or_else(|| QuotesError::Decode("code=0 但 data 为空".into()))?;

    Ok(data
        .items
        .into_iter()
        .map(|row| {
            data.fields
                .iter()
                .zip(row.into_iter())
                .map(|(k, v)| (k.clone(), v))
                .collect()
        })
        .collect())
}

/// 业务错误 code 映射——细分 RateLimited / QuotaExceeded / Provider 三种语义。
fn map_business_error(code: i64, msg: Option<String>) -> QuotesError {
    let msg = msg.unwrap_or_else(|| "（无错误信息）".into());
    match code {
        40203 | 40211 => QuotesError::RateLimited,
        40202 | 40219 => QuotesError::QuotaExceeded,
        _ => QuotesError::Provider {
            provider: "tushare",
            code: Some(code),
            msg,
        },
    }
}

// ============================================================================
// 行记录便利 getter（field 取值容错）
// ============================================================================

pub(crate) fn row_str(row: &HashMap<String, Value>, key: &str) -> Option<String> {
    row.get(key).and_then(|v| v.as_str().map(String::from))
}

pub(crate) fn row_f64(row: &HashMap<String, Value>, key: &str) -> Option<f64> {
    let v = row.get(key)?;
    if let Some(n) = v.as_f64() {
        if n.is_finite() {
            return Some(n);
        }
    }
    v.as_str().and_then(|s| s.parse::<f64>().ok())
}

pub(crate) fn row_i64(row: &HashMap<String, Value>, key: &str) -> Option<i64> {
    let v = row.get(key)?;
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    v.as_str().and_then(|s| s.parse::<i64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn business_error_mapping() {
        assert!(matches!(
            map_business_error(40203, None),
            QuotesError::RateLimited
        ));
        assert!(matches!(
            map_business_error(40219, None),
            QuotesError::QuotaExceeded
        ));
        assert!(matches!(
            map_business_error(99999, Some("x".into())),
            QuotesError::Provider {
                code: Some(99999),
                ..
            }
        ));
    }
}
