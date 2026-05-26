//! 稳定 ID 计算（spec §2 稳定 ID 规则）。
//!
//! Spec: docs/design/news-module.md §2
//!
//! 优先级：
//! 1. 有 URL → `source:url:sha256(canonical_url)`
//! 2. 无 URL，provider 提供稳定 item id → `source:item:sha256(provider_item_id)`
//! 3. 无 URL 且无稳定 item id → `source:fingerprint:sha256(normalized_title + ...)`
//!
//! 不允许使用 fetch time / batchId 等刷新时刻字段参与 fingerprint。

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

/// 计算 SHA-256 输入；输入为 UTF-8 字符串，输出为小写 hex。
fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    let mut out = String::with_capacity(result.len() * 2);
    for b in result {
        use std::fmt::Write;
        let _ = write!(&mut out, "{:02x}", b);
    }
    out
}

/// 生成稳定 ID 的输入。
///
/// `source` 必须是已 normalize 的 `namespace:channel` 格式；
/// `canonical_url` 已通过 [`super::canonicalize_url`] 处理；
/// `provider_item_id` 是 provider 自家稳定 ID；
/// `title` / `summary` / `published_at` 用于 fingerprint 路径。
#[derive(Debug, Clone)]
pub struct IdInput<'a> {
    pub source: &'a str,
    pub canonical_url: Option<&'a str>,
    pub provider_item_id: Option<&'a str>,
    pub title: Option<&'a str>,
    pub summary: Option<&'a str>,
    pub published_at: Option<DateTime<Utc>>,
}

/// 计算稳定 ID。返回 `None` 表示 title 和其他 fingerprint 字段都不足，调用方
/// 应当 skip 该 item 并记录 warning（spec §2）。
pub fn compute_stable_id(input: &IdInput<'_>) -> Option<String> {
    if let Some(url) = input.canonical_url {
        if !url.is_empty() {
            return Some(format!("{}:url:{}", input.source, sha256_hex(url)));
        }
    }
    if let Some(item) = input.provider_item_id {
        let trimmed = item.trim();
        if !trimmed.is_empty() {
            return Some(format!("{}:item:{}", input.source, sha256_hex(trimmed)));
        }
    }
    // fingerprint path —— 必须至少有 title
    let title = input.title.map(|t| t.trim()).unwrap_or("");
    if title.is_empty() {
        return None;
    }
    let title_norm = normalize_text(title);
    let summary_norm = input
        .summary
        .map(|s| normalize_text(s))
        .unwrap_or_default();
    // 按 UTC 秒级精度
    let published_norm = input
        .published_at
        .map(|t| t.timestamp().to_string())
        .unwrap_or_default();

    let buf = format!("{}\u{1f}{}\u{1f}{}", title_norm, published_norm, summary_norm);
    Some(format!("{}:fingerprint:{}", input.source, sha256_hex(&buf)))
}

/// 折叠空白，trim。中文按字符保留；不做大小写折叠（标题大小写本身可能是语义）。
fn normalize_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_ws && !out.is_empty() {
                out.push(' ');
            }
            last_ws = true;
        } else {
            out.push(ch);
            last_ws = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_path_takes_priority() {
        let id = compute_stable_id(&IdInput {
            source: "rss:test",
            canonical_url: Some("https://a.com/x"),
            provider_item_id: Some("ignored"),
            title: Some("t"),
            summary: None,
            published_at: None,
        })
        .unwrap();
        assert!(id.starts_with("rss:test:url:"));
        assert_eq!(id.len(), "rss:test:url:".len() + 64);
    }

    #[test]
    fn falls_back_to_item_id_when_no_url() {
        let id = compute_stable_id(&IdInput {
            source: "newsnow:hot",
            canonical_url: None,
            provider_item_id: Some("abc"),
            title: Some("t"),
            summary: None,
            published_at: None,
        })
        .unwrap();
        assert!(id.starts_with("newsnow:hot:item:"));
    }

    #[test]
    fn fingerprint_path_requires_title() {
        assert!(compute_stable_id(&IdInput {
            source: "rss:x",
            canonical_url: None,
            provider_item_id: None,
            title: None,
            summary: Some("s"),
            published_at: None,
        })
        .is_none());
    }

    #[test]
    fn fingerprint_stable_across_whitespace() {
        let a = compute_stable_id(&IdInput {
            source: "rss:x",
            canonical_url: None,
            provider_item_id: None,
            title: Some("Hello  World"),
            summary: None,
            published_at: None,
        })
        .unwrap();
        let b = compute_stable_id(&IdInput {
            source: "rss:x",
            canonical_url: None,
            provider_item_id: None,
            title: Some(" Hello World "),
            summary: None,
            published_at: None,
        })
        .unwrap();
        assert_eq!(a, b);
    }
}
