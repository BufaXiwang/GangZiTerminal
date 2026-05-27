//! TuShare 公司事件：dividend / suspend_d / namechange / forecast / share_float。
//!
//! Spec: docs/design/quotes-module.md §2 / §5；docs/design/references/quotes/tushare.md

use super::client::{pick_str, TushareClient, TushareError};
use crate::domain::quotes::quote::{CompanyEvent, CompanyEventType};
use crate::domain::shared::{TradeDate, TsCode};
use chrono::Utc;
use serde_json::{json, Value as Json};
use sha2::{Digest, Sha256};

fn event_id(prefix: &str, parts: &[&str]) -> String {
    let mut h = Sha256::new();
    h.update(prefix.as_bytes());
    for p in parts {
        h.update(b"|");
        h.update(p.as_bytes());
    }
    let digest = h.finalize();
    format!("{}-{:016x}", prefix, u64::from_be_bytes(digest[..8].try_into().unwrap()))
}

impl TushareClient {
    /// 分红送股事件。
    pub async fn fetch_dividends(
        &self,
        ts_code: Option<&TsCode>,
        ann_start: &str,
        ann_end: &str,
    ) -> Result<Vec<CompanyEvent>, TushareError> {
        let mut params = json!({"ann_date_start": ann_start, "ann_date_end": ann_end});
        if let Some(c) = ts_code {
            params["ts_code"] = json!(c.as_str());
        }
        let data = self
            .call(
                "dividend",
                params,
                "ts_code,ann_date,end_date,div_proc,record_date,ex_date",
            )
            .await?;
        let now = Utc::now();
        let mut out = Vec::with_capacity(data.items.len());
        for row in data.items.iter() {
            let Some(ts) = pick_str(&data.fields, row, "ts_code") else {
                continue;
            };
            let Ok(ts_code) = TsCode::parse(&ts) else { continue };
            let ann = pick_str(&data.fields, row, "ann_date");
            let ex = pick_str(&data.fields, row, "ex_date");
            let id = event_id(
                "div",
                &[ts_code.as_str(), ann.as_deref().unwrap_or(""), ex.as_deref().unwrap_or("")],
            );
            let payload = build_payload(&data.fields, row);
            out.push(CompanyEvent {
                id,
                ts_code,
                event_type: CompanyEventType::Dividend,
                announce_date: ann.and_then(|s| TradeDate::parse(&s).ok()),
                effective_date: ex.and_then(|s| TradeDate::parse(&s).ok()),
                payload,
                source: "tushare".to_string(),
                fetched_at: now,
            });
        }
        Ok(out)
    }

    /// 停复牌事件。
    pub async fn fetch_suspensions(
        &self,
        ts_code: Option<&TsCode>,
        suspend_date_start: &str,
        suspend_date_end: &str,
    ) -> Result<Vec<CompanyEvent>, TushareError> {
        let mut params = json!({
            "start_date": suspend_date_start,
            "end_date": suspend_date_end,
        });
        if let Some(c) = ts_code {
            params["ts_code"] = json!(c.as_str());
        }
        let data = self
            .call(
                "suspend_d",
                params,
                "ts_code,trade_date,suspend_type,suspend_timing",
            )
            .await?;
        let now = Utc::now();
        let mut out = Vec::with_capacity(data.items.len());
        for row in data.items.iter() {
            let Some(ts) = pick_str(&data.fields, row, "ts_code") else {
                continue;
            };
            let Ok(ts_code) = TsCode::parse(&ts) else { continue };
            let td = pick_str(&data.fields, row, "trade_date");
            let suspend_type = pick_str(&data.fields, row, "suspend_type").unwrap_or_default();
            let event_type = if suspend_type == "R" {
                CompanyEventType::Resume
            } else {
                CompanyEventType::Suspension
            };
            let id = event_id(
                "susp",
                &[ts_code.as_str(), td.as_deref().unwrap_or(""), &suspend_type],
            );
            let payload = build_payload(&data.fields, row);
            out.push(CompanyEvent {
                id,
                ts_code,
                event_type,
                announce_date: td.clone().and_then(|s| TradeDate::parse(&s).ok()),
                effective_date: td.and_then(|s| TradeDate::parse(&s).ok()),
                payload,
                source: "tushare".to_string(),
                fetched_at: now,
            });
        }
        Ok(out)
    }
}

fn build_payload(fields: &[String], row: &[Json]) -> Json {
    let mut obj = serde_json::Map::new();
    for (i, f) in fields.iter().enumerate() {
        if let Some(v) = row.get(i) {
            obj.insert(f.clone(), v.clone());
        }
    }
    Json::Object(obj)
}
