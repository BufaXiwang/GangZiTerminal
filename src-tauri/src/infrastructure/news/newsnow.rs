//! NewsNow provider — 高频聚合资讯。
//!
//! Spec: docs/design/references/news/newsnow.md
//!
//! 当前阶段：NewsNow endpoint 没有内置默认部署；adapter 只在 source 配置了 `feed_url`
//! （指向 NewsNow 实例的 channel JSON endpoint）时才会拉取。
//!
//! 这是非阻塞性 spec 留白：NewsNow upstream wire schema 没有官方稳定文档，本 adapter
//! 解析最常见的 `{ items: [{ id, title, url, time, summary, ... }] }` 形状。
//! TODO(spec-feedback): NewsNow upstream payload schema 应该作为 reference 文档补全。

use crate::domain::news::canonical_url::canonicalize_url;
use crate::domain::news::events::{NewsFailure, NewsRefreshStage, NewsRefreshWarning};
use crate::domain::news::ids::{compute_stable_id, IdInput};
use crate::domain::news::source::NewsSourceRef;
use crate::domain::news::types::ProviderNewsItem;
use crate::domain::shared::{ErrorCode, WarningCode};
use chrono::{DateTime, TimeZone, Utc};
use reqwest::Client;
use std::time::Duration;

pub const NEWSNOW_TIMEOUT_SECS: u64 = 8;
pub const NEWSNOW_MAX_ITEMS: usize = 100;

// 浏览器 UA：NewsNow 公开实例 (newsnow.busiyi.world) 对未带 Origin / 非浏览器 UA
// 直接 403。其他自部署实例不需要这种伪装，但发个标准 UA 也没副作用。
const BROWSER_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/120.0 Safari/537.36";

pub struct NewsNowProvider {
    client: Client,
}

impl NewsNowProvider {
    pub fn new() -> reqwest::Result<Self> {
        let client = Client::builder()
            .user_agent(BROWSER_USER_AGENT)
            .timeout(Duration::from_secs(NEWSNOW_TIMEOUT_SECS))
            .build()?;
        Ok(Self { client })
    }

    pub async fn fetch(
        &self,
        source: &NewsSourceRef,
    ) -> (Vec<ProviderNewsItem>, Vec<NewsRefreshWarning>, Option<NewsFailure>) {
        let now = Utc::now();
        let endpoint = match source.feed_url.as_deref() {
            Some(u) => u,
            None => {
                return (
                    vec![],
                    vec![],
                    Some(NewsFailure {
                        provider: "newsnow".to_string(),
                        source: Some(source.source_id.clone()),
                        code: ErrorCode::InvalidInput,
                        message: Some("newsnow source missing endpoint feed_url".to_string()),
                        details: None,
                        stage: Some(NewsRefreshStage::Fetch),
                        retryable: Some(false),
                        occurred_at: now,
                    }),
                );
            }
        };

        // NewsNow 服务端要求 Origin 同源；从 endpoint URL 派生 scheme://host[:port]。
        // 派生失败时跳过 header，让服务端用默认行为（自部署实例可能不需要）。
        let origin = derive_origin(endpoint);
        let mut req = self.client.get(endpoint);
        if let Some(o) = origin.as_deref() {
            req = req.header("Origin", o).header("Referer", o);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                return (
                    vec![],
                    vec![],
                    Some(NewsFailure {
                        provider: "newsnow".to_string(),
                        source: Some(source.source_id.clone()),
                        code: ErrorCode::ProviderUnavailable,
                        message: Some(e.to_string()),
                        details: None,
                        stage: Some(NewsRefreshStage::Fetch),
                        retryable: Some(true),
                        occurred_at: now,
                    }),
                );
            }
        };

        if resp.status().as_u16() == 429 {
            return (
                vec![],
                vec![],
                Some(NewsFailure {
                    provider: "newsnow".to_string(),
                    source: Some(source.source_id.clone()),
                    code: ErrorCode::RateLimited,
                    message: Some("newsnow returned 429".to_string()),
                    details: None,
                    stage: Some(NewsRefreshStage::Fetch),
                    retryable: Some(true),
                    occurred_at: now,
                }),
            );
        }

        if !resp.status().is_success() {
            return (
                vec![],
                vec![],
                Some(NewsFailure {
                    provider: "newsnow".to_string(),
                    source: Some(source.source_id.clone()),
                    code: ErrorCode::ProviderUnavailable,
                    message: Some(format!("newsnow http status {}", resp.status())),
                    details: None,
                    stage: Some(NewsRefreshStage::Fetch),
                    retryable: Some(true),
                    occurred_at: now,
                }),
            );
        }

        let json: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return (
                    vec![],
                    vec![],
                    Some(NewsFailure {
                        provider: "newsnow".to_string(),
                        source: Some(source.source_id.clone()),
                        code: ErrorCode::ParseError,
                        message: Some(e.to_string()),
                        details: None,
                        stage: Some(NewsRefreshStage::Normalize),
                        retryable: Some(false),
                        occurred_at: now,
                    }),
                );
            }
        };

        let (items, warnings) = normalize_payload(&source.source_id, &json);
        (items, warnings, None)
    }
}

fn normalize_payload(
    source_id: &str,
    payload: &serde_json::Value,
) -> (Vec<ProviderNewsItem>, Vec<NewsRefreshWarning>) {
    let now = Utc::now();
    // 支持两种最常见形状：根数组 / { items: [...] }
    let arr = payload
        .as_array()
        .or_else(|| payload.get("items").and_then(|v| v.as_array()));
    let arr = match arr {
        Some(a) => a,
        None => {
            return (
                vec![],
                vec![NewsRefreshWarning {
                    provider: "newsnow".to_string(),
                    source: Some(source_id.to_string()),
                    code: WarningCode::DataPartial,
                    message: Some("newsnow payload not array / {items:[]}".to_string()),
                    stage: Some(NewsRefreshStage::Normalize),
                    skipped_count: None,
                    occurred_at: now,
                }],
            );
        }
    };

    let mut out = Vec::new();
    let mut skipped = 0u32;

    for raw in arr.iter().take(NEWSNOW_MAX_ITEMS) {
        let title = pick_string(raw, &["title", "name"]);
        let title = match title {
            Some(t) if !t.trim().is_empty() => t,
            _ => {
                skipped += 1;
                continue;
            }
        };
        let provider_item_id =
            pick_string(raw, &["id", "_id", "guid", "uuid"]).map(|s| s.to_string());
        let raw_url = pick_string(raw, &["url", "link"]).map(|s| s.to_string());
        let canonical = raw_url
            .as_deref()
            .and_then(|u| canonicalize_url(u).ok());
        let summary = pick_string(raw, &["summary", "description", "desc"]).map(|s| s.to_string());
        // 时间字段三种来源（NewsNow channel 各异，见 references/news/newsnow.md）：
        //   - cls-telegraph: 顶层 pubDate = 数字毫秒
        //   - jin10:         顶层 pubDate = 北京时间字符串 "YYYY-MM-DD HH:MM:SS"
        //   - wallstreetcn:  嵌套 extra.date = 数字毫秒
        let published_at = pick_time(raw, &["publishedAt", "time", "pubDate", "published"])
            .or_else(|| raw.get("extra").and_then(|e| pick_time(e, &["date", "time"])));

        let id_input = IdInput {
            source: source_id,
            canonical_url: canonical.as_deref(),
            provider_item_id: provider_item_id.as_deref(),
            title: Some(&title),
            summary: summary.as_deref(),
            published_at,
        };
        let id = match compute_stable_id(&id_input) {
            Some(i) => i,
            None => {
                skipped += 1;
                continue;
            }
        };

        let mut payload_obj = serde_json::Map::new();
        payload_obj.insert("provider".into(), serde_json::Value::String("newsnow".into()));
        payload_obj.insert("raw".into(), raw.clone());
        if let Some(media) = pick_string(raw, &["media", "publisher", "source"]) {
            payload_obj.insert(
                "media".into(),
                serde_json::Value::String(media.to_string()),
            );
        }
        if let (Some(raw_u), Some(canon)) = (raw_url.as_deref(), canonical.as_deref()) {
            if raw_u != canon {
                payload_obj.insert(
                    "originalUrl".into(),
                    serde_json::Value::String(raw_u.to_string()),
                );
            }
        }

        out.push(ProviderNewsItem {
            id,
            source: source_id.to_string(),
            title: title.to_string(),
            summary,
            url: canonical,
            published_at,
            payload: serde_json::Value::Object(payload_obj),
        });
    }

    let mut warnings = Vec::new();
    if skipped > 0 {
        warnings.push(NewsRefreshWarning {
            provider: "newsnow".to_string(),
            source: Some(source_id.to_string()),
            code: WarningCode::DataPartial,
            message: Some("newsnow items skipped".to_string()),
            stage: Some(NewsRefreshStage::Normalize),
            skipped_count: Some(skipped),
            occurred_at: now,
        });
    }
    (out, warnings)
}

/// 从 `https://host[:port]/path?query` 派生 `https://host[:port]`。
/// 不依赖 url crate；newsnow endpoint 形态稳定够用。
fn derive_origin(endpoint: &str) -> Option<String> {
    let (scheme, rest) = endpoint.split_once("://")?;
    let host_port = rest.split(['/', '?', '#']).next()?;
    if host_port.is_empty() {
        return None;
    }
    Some(format!("{}://{}", scheme, host_port))
}

#[cfg(test)]
mod origin_tests {
    use super::derive_origin;

    #[test]
    fn derives_origin_from_full_url() {
        assert_eq!(
            derive_origin("https://newsnow.busiyi.world/api/s?id=cls-telegraph&latest"),
            Some("https://newsnow.busiyi.world".to_string())
        );
    }

    #[test]
    fn derives_origin_with_port() {
        assert_eq!(
            derive_origin("http://localhost:3000/api/s?id=x"),
            Some("http://localhost:3000".to_string())
        );
    }

    #[test]
    fn returns_none_for_invalid() {
        assert_eq!(derive_origin("not-a-url"), None);
    }
}

#[cfg(test)]
mod time_tests {
    use super::{parse_time_str, pick_time};

    #[test]
    fn parses_beijing_naive_string_as_utc8() {
        // 2026-05-29 11:31:24 北京 = 2026-05-29 03:31:24 UTC
        let dt = parse_time_str("2026-05-29 11:31:24").expect("parse");
        assert_eq!(dt.to_rfc3339(), "2026-05-29T03:31:24+00:00");
    }

    #[test]
    fn parses_millis_string() {
        let dt = parse_time_str("1779953515000").expect("parse");
        assert_eq!(dt.timestamp_millis(), 1779953515000);
    }

    #[test]
    fn parses_rfc3339() {
        let dt = parse_time_str("2026-05-29T11:31:24+08:00").expect("parse");
        assert_eq!(dt.to_rfc3339(), "2026-05-29T03:31:24+00:00");
    }

    #[test]
    fn picks_nested_extra_date() {
        // wallstreetcn 形态：extra.date 数字毫秒
        let raw = serde_json::json!({ "title": "x", "extra": { "date": 1780024789000_i64 } });
        let dt = raw
            .get("extra")
            .and_then(|e| pick_time(e, &["date", "time"]))
            .expect("extra.date");
        assert_eq!(dt.timestamp_millis(), 1780024789000);
    }
}

fn pick_string<'a>(v: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

fn pick_time(v: &serde_json::Value, keys: &[&str]) -> Option<DateTime<Utc>> {
    for k in keys {
        if let Some(field) = v.get(*k) {
            if let Some(s) = field.as_str() {
                if let Some(dt) = parse_time_str(s) {
                    return Some(dt);
                }
            } else if let Some(num) = field.as_i64() {
                return Some(ms_to_dt(num));
            } else if let Some(num) = field.as_f64() {
                return Some(ms_to_dt(num as i64));
            }
        }
    }
    None
}

/// 解析 NewsNow 各 channel 的字符串时间。支持：
/// - RFC3339 (`2026-05-29T11:31:24+08:00` / `...Z`)
/// - 数字毫秒 / 秒字符串
/// - 北京时间裸字符串 `YYYY-MM-DD HH:MM:SS`（jin10）—— 无时区，按 UTC+8 解释
fn parse_time_str(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(d.with_timezone(&Utc));
    }
    if let Ok(n) = s.parse::<i64>() {
        return Some(ms_to_dt(n));
    }
    // 北京时间裸字符串：解析为 NaiveDateTime 再减 8h 得 UTC。
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        let beijing = naive - chrono::Duration::hours(8);
        return Some(DateTime::from_naive_utc_and_offset(beijing, Utc));
    }
    None
}

fn ms_to_dt(value: i64) -> DateTime<Utc> {
    // 若数值小于 1e12 视为秒；否则视为毫秒
    if value.abs() < 1_000_000_000_000 {
        Utc.timestamp_opt(value, 0).single().unwrap_or_else(Utc::now)
    } else {
        Utc.timestamp_millis_opt(value).single().unwrap_or_else(Utc::now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_handles_items_wrapper() {
        let p = serde_json::json!({
            "items": [
                {"id": "1", "title": "Hello", "url": "https://a.com/x?utm_source=x", "time": 1735689600000_i64 },
                {"id": "2", "title": ""},
            ]
        });
        let (items, warnings) = normalize_payload("newsnow:hot", &p);
        assert_eq!(items.len(), 1);
        assert!(items[0].url.as_deref() == Some("https://a.com/x"));
        assert!(warnings.iter().any(|w| w.skipped_count == Some(1)));
    }
}
