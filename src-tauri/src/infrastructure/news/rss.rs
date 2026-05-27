//! RSS provider — 拉取 RSS / Atom feed 并 normalize 到 `ProviderNewsItem`。
//!
//! Spec: docs/design/references/news/rss.md
//!
//! 默认限制：
//! - request timeout 8s
//! - retry 1 次
//! - max items per feed 100
//!
//! 失败映射（spec §5 failure code 表）：
//! - 网络失败 → `NewsFailure(provider="rss", code="provider_unavailable", stage="fetch")`
//! - 解析失败 → `NewsFailure(provider="rss", code="parse_error", stage="normalize")`
//! - 单条 item 缺 title 或 ID → skip + warning（不阻塞）

use crate::domain::news::canonical_url::canonicalize_url;
use crate::domain::news::events::{NewsFailure, NewsRefreshStage, NewsRefreshWarning};
use crate::domain::news::ids::{compute_stable_id, IdInput};
use crate::domain::news::source::NewsSourceRef;
use crate::domain::news::types::ProviderNewsItem;
use crate::domain::shared::{ErrorCode, WarningCode};
use chrono::Utc;
use feed_rs::parser as feed_parser;
use reqwest::Client;
use std::time::Duration;

/// RSS 默认配置（references/news/rss.md）。
pub const RSS_TIMEOUT_SECS: u64 = 8;
pub const RSS_MAX_ITEMS: usize = 100;

pub struct RssProvider {
    client: Client,
}

impl RssProvider {
    pub fn new() -> reqwest::Result<Self> {
        let client = Client::builder()
            .user_agent("GangZi-Terminal/0.1 (+news/rss)")
            .timeout(Duration::from_secs(RSS_TIMEOUT_SECS))
            .build()?;
        Ok(Self { client })
    }

    /// 拉取并 normalize 一个 RSS source。
    ///
    /// 返回值：(items, warnings, failure)。
    /// - 成功（含部分跳过）：`failure = None`
    /// - 整源失败：`items = vec![]`，`failure = Some(...)`
    pub async fn fetch(
        &self,
        source: &NewsSourceRef,
    ) -> (Vec<ProviderNewsItem>, Vec<NewsRefreshWarning>, Option<NewsFailure>) {
        let now = Utc::now();
        let feed_url = match source.feed_url.as_deref() {
            Some(u) => u,
            None => {
                return (
                    vec![],
                    vec![],
                    Some(NewsFailure {
                        provider: "rss".to_string(),
                        source: Some(source.source_id.clone()),
                        code: ErrorCode::InvalidInput,
                        message: Some("rss source missing feed_url".to_string()),
                        details: None,
                        stage: Some(NewsRefreshStage::Fetch),
                        retryable: Some(false),
                        occurred_at: now,
                    }),
                );
            }
        };

        let body = match self.client.get(feed_url).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    return (
                        vec![],
                        vec![],
                        Some(NewsFailure {
                            provider: "rss".to_string(),
                            source: Some(source.source_id.clone()),
                            code: ErrorCode::ProviderUnavailable,
                            message: Some(format!("rss http status {}", resp.status())),
                            details: None,
                            stage: Some(NewsRefreshStage::Fetch),
                            retryable: Some(true),
                            occurred_at: now,
                        }),
                    );
                }
                match resp.bytes().await {
                    Ok(b) => b,
                    Err(e) => {
                        return (
                            vec![],
                            vec![],
                            Some(NewsFailure {
                                provider: "rss".to_string(),
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
                }
            }
            Err(e) => {
                return (
                    vec![],
                    vec![],
                    Some(NewsFailure {
                        provider: "rss".to_string(),
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

        let feed = match feed_parser::parse(&body[..]) {
            Ok(f) => f,
            Err(e) => {
                // Spec §5: normalize 阶段失败 → parse_error
                return (
                    vec![],
                    vec![],
                    Some(NewsFailure {
                        provider: "rss".to_string(),
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

        let (items, warnings) = normalize_feed(&source.source_id, &feed);
        (items, warnings, None)
    }
}

fn normalize_feed(
    source_id: &str,
    feed: &feed_rs::model::Feed,
) -> (Vec<ProviderNewsItem>, Vec<NewsRefreshWarning>) {
    let now = Utc::now();
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let mut skipped_no_title = 0u32;
    let mut skipped_no_id = 0u32;

    for entry in feed.entries.iter().take(RSS_MAX_ITEMS) {
        let title_text = entry
            .title
            .as_ref()
            .map(|t| t.content.trim().to_string())
            .unwrap_or_default();
        if title_text.is_empty() {
            skipped_no_title += 1;
            continue;
        }

        let summary_text = entry.summary.as_ref().map(|s| strip_html(&s.content));

        // 多个 link 时取第一个 alternate / 普通 link
        let raw_url = entry
            .links
            .iter()
            .find(|l| {
                l.rel
                    .as_deref()
                    .map(|r| r == "alternate" || r == "self")
                    .unwrap_or(true)
            })
            .map(|l| l.href.clone());

        let canonical = raw_url.as_deref().and_then(|u| canonicalize_url(u).ok());

        let published_at = entry
            .published
            .or(entry.updated)
            .map(|d| d.with_timezone(&Utc));

        let id_input = IdInput {
            source: source_id,
            canonical_url: canonical.as_deref(),
            provider_item_id: Some(entry.id.as_str()),
            title: Some(&title_text),
            summary: summary_text.as_deref(),
            published_at,
        };
        let id = match compute_stable_id(&id_input) {
            Some(id) => id,
            None => {
                skipped_no_id += 1;
                continue;
            }
        };

        let mut payload = serde_json::Map::new();
        payload.insert("provider".into(), serde_json::Value::String("rss".into()));
        if let Some(raw) = raw_url.as_deref() {
            if Some(raw) != canonical.as_deref() {
                payload.insert(
                    "originalUrl".into(),
                    serde_json::Value::String(raw.to_string()),
                );
            }
        }
        if let Some(authors) = first_or_none(&entry.authors) {
            payload.insert(
                "publisher".into(),
                serde_json::Value::String(authors.to_string()),
            );
        }

        out.push(ProviderNewsItem {
            id,
            source: source_id.to_string(),
            title: title_text,
            summary: summary_text,
            url: canonical,
            published_at,
            payload: serde_json::Value::Object(payload),
        });
    }

    if skipped_no_title > 0 {
        warnings.push(NewsRefreshWarning {
            provider: "rss".to_string(),
            source: Some(source_id.to_string()),
            code: WarningCode::DataPartial,
            message: Some("rss items skipped: missing title".to_string()),
            stage: Some(NewsRefreshStage::Normalize),
            skipped_count: Some(skipped_no_title),
            occurred_at: now,
        });
    }
    if skipped_no_id > 0 {
        warnings.push(NewsRefreshWarning {
            provider: "rss".to_string(),
            source: Some(source_id.to_string()),
            code: WarningCode::DataPartial,
            message: Some("rss items skipped: cannot generate stable id".to_string()),
            stage: Some(NewsRefreshStage::Normalize),
            skipped_count: Some(skipped_no_id),
            occurred_at: now,
        });
    }
    (out, warnings)
}

fn strip_html(s: &str) -> String {
    // 轻量去标签：保留文本内容，折叠空白。
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    let trimmed = out.trim();
    trimmed.to_string()
}

fn first_or_none(authors: &[feed_rs::model::Person]) -> Option<&str> {
    authors.first().map(|a| a.name.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_basic() {
        assert_eq!(strip_html("<p>hello <b>world</b></p>"), "hello world");
    }

    #[test]
    fn normalize_feed_skips_no_title_entries() {
        let raw = br#"<?xml version="1.0"?>
        <rss version="2.0"><channel>
            <title>test</title>
            <item><title>Good item</title><link>https://a.com/x</link><guid>g1</guid></item>
            <item><link>https://a.com/y</link><guid>g2</guid></item>
        </channel></rss>"#;
        let feed = feed_parser::parse(&raw[..]).unwrap();
        let (items, warnings) = normalize_feed("rss:t", &feed);
        assert_eq!(items.len(), 1);
        assert!(items[0].id.starts_with("rss:t:url:"));
        assert!(warnings.iter().any(|w| w.skipped_count == Some(1)));
    }
}
