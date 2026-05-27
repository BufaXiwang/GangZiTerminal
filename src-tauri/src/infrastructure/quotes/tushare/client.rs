//! TuShare Pro 统一 HTTP client。
//!
//! Spec: docs/design/quotes-module.md §5；docs/design/references/quotes/tushare.md
//!
//! 接口模式：POST `https://api.tushare.pro` body = `{api_name, token, params, fields}`，
//! 响应 `{code, msg, data: {fields, items}}`。
//!
//! token 缺失时构造仍可以；调用接口时返回 `TokenMissing` 错误，调用方按 spec 跳过 enrich。

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::time::Duration;
use thiserror::Error;

const TIMEOUT: Duration = Duration::from_secs(10);
const API: &str = "https://api.tushare.pro";

#[derive(Debug, Error)]
pub enum TushareError {
    #[error("tushare token missing")]
    TokenMissing,
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("tushare api error: code={code} msg={msg}")]
    Api { code: i64, msg: String },
    #[error("parse: {0}")]
    Parse(String),
}

#[derive(Clone)]
pub struct TushareClient {
    http: Client,
    token: Option<String>,
}

#[derive(Serialize)]
struct Request<'a> {
    api_name: &'a str,
    token: &'a str,
    params: Json,
    fields: &'a str,
}

#[derive(Deserialize)]
struct Response {
    code: i64,
    msg: Option<String>,
    data: Option<RespData>,
}

#[derive(Deserialize)]
pub struct RespData {
    pub fields: Vec<String>,
    pub items: Vec<Vec<Json>>,
}

impl TushareClient {
    pub fn new(token: Option<String>) -> reqwest::Result<Self> {
        let http = Client::builder().timeout(TIMEOUT).build()?;
        Ok(Self { http, token })
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// 通用调用入口。
    pub async fn call(
        &self,
        api_name: &str,
        params: Json,
        fields: &str,
    ) -> Result<RespData, TushareError> {
        let token = self.token.as_deref().ok_or(TushareError::TokenMissing)?;
        let body = Request {
            api_name,
            token,
            params,
            fields,
        };
        let resp = self
            .http
            .post(API)
            .json(&body)
            .send()
            .await?
            .json::<Response>()
            .await
            .map_err(|e| TushareError::Parse(e.to_string()))?;
        if resp.code != 0 {
            return Err(TushareError::Api {
                code: resp.code,
                msg: resp.msg.unwrap_or_default(),
            });
        }
        resp.data
            .ok_or_else(|| TushareError::Parse("data is null".into()))
    }
}

/// 工具：从一行 items 按字段名取值。
pub fn pick<'a>(fields: &[String], row: &'a [Json], name: &str) -> Option<&'a Json> {
    let idx = fields.iter().position(|f| f == name)?;
    row.get(idx)
}

pub fn pick_str(fields: &[String], row: &[Json], name: &str) -> Option<String> {
    pick(fields, row, name).and_then(|v| match v {
        Json::String(s) => Some(s.clone()),
        Json::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

pub fn pick_f64(fields: &[String], row: &[Json], name: &str) -> Option<f64> {
    pick(fields, row, name).and_then(|v| match v {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => s.parse::<f64>().ok(),
        _ => None,
    })
}

pub fn pick_i64(fields: &[String], row: &[Json], name: &str) -> Option<i64> {
    pick(fields, row, name).and_then(|v| match v {
        Json::Number(n) => n.as_i64(),
        Json::String(s) => s.parse::<i64>().ok(),
        _ => None,
    })
}
