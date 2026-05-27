//! Article extractor — 按 canonical URL 抓正文 + 抽 main content。
//!
//! Spec: docs/design/references/news/article-extractor.md
//!
//! 默认限制：
//! - request timeout 10s
//! - max body size 2MB
//! - retry 1 次（暂未实现 retry，留 TODO）

use crate::domain::news::types::ArticleContent;
use crate::domain::shared::{ErrorCode, OccurredAt, WarningCode};
use chrono::Utc;
use reqwest::Client;
use scraper::{Html, Selector};
use std::time::Duration;

pub const ARTICLE_TIMEOUT_SECS: u64 = 10;
pub const ARTICLE_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// 太短的正文视为抽取失败（spec §抽取规则）。
pub const ARTICLE_MIN_CONTENT_CHARS: usize = 80;

/// 抽取结果。`error` 仅在彻底失败（fetch / parse）时填充，调用方据此映射到
/// `NewsFailure(stage="article")`。
///
/// Spec: news-module.md §5 failure code 表 — article stage 失败统一 code = `article_extract_failed`，
/// 细分原因放 `reason`，供 service 层写入 `NewsFailure.details.reason`。
pub struct ArticleExtractOutput {
    pub article: ArticleContent,
    /// `(code, reason, message)`：`code` 固定为 `ArticleExtractFailed`；`reason` 为细分原因
    /// （`network` / `timeout` / `too_short` / `unsupported_content_type` / `http_status`
    /// / `parse_error`），调用方写入 `NewsFailure.details.reason`。
    pub error: Option<ArticleExtractFailure>,
}

#[derive(Debug, Clone)]
pub struct ArticleExtractFailure {
    pub code: ErrorCode,
    pub reason: ArticleExtractReason,
    pub message: String,
}

/// article-stage 细分原因（写入 `NewsFailure.details.reason`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArticleExtractReason {
    Network,
    Timeout,
    TooShort,
    UnsupportedContentType,
    HttpStatus,
    ParseError,
}

impl ArticleExtractReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ArticleExtractReason::Network => "network",
            ArticleExtractReason::Timeout => "timeout",
            ArticleExtractReason::TooShort => "too_short",
            ArticleExtractReason::UnsupportedContentType => "unsupported_content_type",
            ArticleExtractReason::HttpStatus => "http_status",
            ArticleExtractReason::ParseError => "parse_error",
        }
    }
}

pub struct ArticleExtractor {
    client: Client,
}

impl ArticleExtractor {
    pub fn new() -> reqwest::Result<Self> {
        let client = Client::builder()
            .user_agent(
                "Mozilla/5.0 (compatible; GangZi-Terminal/0.1; +news/article-extractor)",
            )
            .timeout(Duration::from_secs(ARTICLE_TIMEOUT_SECS))
            .build()?;
        Ok(Self { client })
    }

    /// 抽取 `canonical_url` 对应的正文。`first_news_id` 用于审计。
    pub async fn extract(&self, canonical_url: &str, first_news_id: Option<&str>) -> ArticleExtractOutput {
        let now = Utc::now();
        match self.do_extract(canonical_url, first_news_id, now).await {
            Ok(out) => out,
            Err(e) => failure(canonical_url, first_news_id, now, e.reason, &e.message),
        }
    }

    async fn do_extract(
        &self,
        canonical_url: &str,
        first_news_id: Option<&str>,
        now: OccurredAt,
    ) -> Result<ArticleExtractOutput, ExtractErr> {
        let resp = self
            .client
            .get(canonical_url)
            .send()
            .await
            .map_err(|e| ExtractErr {
                reason: if e.is_timeout() {
                    ArticleExtractReason::Timeout
                } else {
                    ArticleExtractReason::Network
                },
                message: e.to_string(),
            })?;

        if !resp.status().is_success() {
            return Err(ExtractErr {
                reason: ArticleExtractReason::HttpStatus,
                message: format!("http status {}", resp.status()),
            });
        }

        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        let body_bytes = resp
            .bytes()
            .await
            .map_err(|e| ExtractErr {
                reason: ArticleExtractReason::Network,
                message: e.to_string(),
            })?;
        if body_bytes.len() > ARTICLE_MAX_BODY_BYTES {
            return Err(ExtractErr {
                reason: ArticleExtractReason::HttpStatus,
                message: format!("body exceeds {} bytes", ARTICLE_MAX_BODY_BYTES),
            });
        }

        // Content-Type 不是 HTML-like：返回失败 + 细分原因 unsupported_content_type
        if !ct.is_empty() && !is_html_like(&ct) {
            return Err(ExtractErr {
                reason: ArticleExtractReason::UnsupportedContentType,
                message: format!("unsupported content-type: {}", ct),
            });
        }

        // 默认按 UTF-8 解析；多数中文站为 UTF-8。
        let html = String::from_utf8_lossy(&body_bytes).into_owned();
        let (title, content) = extract_main(&html);

        let too_short = content
            .as_deref()
            .map(|c| c.chars().count() < ARTICLE_MIN_CONTENT_CHARS)
            .unwrap_or(true);

        if too_short {
            // 失败缓存 + 细分原因 too_short（spec §5 article stage failure 必填 reason）
            let article = ArticleContent {
                url: canonical_url.to_string(),
                first_news_id: first_news_id.map(|s| s.to_string()),
                title,
                content: None,
                payload: serde_json::json!({
                    "provider": "article_extractor",
                    "reason": ArticleExtractReason::TooShort.as_str(),
                }),
                fetched_at: now,
                warning: Some(WarningCode::ArticleMissing),
            };
            return Ok(ArticleExtractOutput {
                article,
                error: Some(ArticleExtractFailure {
                    code: ErrorCode::ArticleExtractFailed,
                    reason: ArticleExtractReason::TooShort,
                    message: "content empty or too short".to_string(),
                }),
            });
        }

        Ok(ArticleExtractOutput {
            article: ArticleContent {
                url: canonical_url.to_string(),
                first_news_id: first_news_id.map(|s| s.to_string()),
                title,
                content,
                payload: serde_json::json!({"provider": "article_extractor"}),
                fetched_at: now,
                warning: None,
            },
            error: None,
        })
    }
}

struct ExtractErr {
    reason: ArticleExtractReason,
    message: String,
}

fn failure(
    canonical_url: &str,
    first_news_id: Option<&str>,
    now: OccurredAt,
    reason: ArticleExtractReason,
    message: &str,
) -> ArticleExtractOutput {
    // 即使失败也保存失败缓存（spec：避免短时间反复抓取）。
    let article = ArticleContent {
        url: canonical_url.to_string(),
        first_news_id: first_news_id.map(|s| s.to_string()),
        title: None,
        content: None,
        payload: serde_json::json!({
            "provider": "article_extractor",
            "reason": reason.as_str(),
            "error": message,
        }),
        fetched_at: now,
        warning: Some(WarningCode::ArticleMissing),
    };
    ArticleExtractOutput {
        article,
        error: Some(ArticleExtractFailure {
            code: ErrorCode::ArticleExtractFailed,
            reason,
            message: message.to_string(),
        }),
    }
}

fn is_html_like(content_type: &str) -> bool {
    content_type.contains("html")
        || content_type.contains("xhtml")
        || content_type.contains("text/plain")
}

/// 提取标题 + 主正文。简单策略：
/// - title: `<meta property="og:title">` → `<title>`
/// - content: `<article>` → `<main>` → `<body>` 文本，剔除 script/style/nav 等。
pub fn extract_main(html: &str) -> (Option<String>, Option<String>) {
    let doc = Html::parse_document(html);
    let title = pick_title(&doc);
    let body_text = pick_main_text(&doc);
    let cleaned = body_text.map(|t| clean_whitespace(&t));
    (title, cleaned)
}

fn pick_title(doc: &Html) -> Option<String> {
    let og_selector = Selector::parse("meta[property='og:title']").ok()?;
    if let Some(el) = doc.select(&og_selector).next() {
        if let Some(c) = el.value().attr("content") {
            let t = c.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    let title_selector = Selector::parse("title").ok()?;
    if let Some(el) = doc.select(&title_selector).next() {
        let t = el.text().collect::<String>();
        let t = t.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    None
}

fn pick_main_text(doc: &Html) -> Option<String> {
    for sel_str in &["article", "main", "div.article", "div#article", "body"] {
        let sel = match Selector::parse(sel_str) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if let Some(el) = doc.select(&sel).next() {
            let mut buf = String::new();
            collect_text(el, &mut buf);
            let trimmed = buf.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn collect_text(el: scraper::ElementRef<'_>, buf: &mut String) {
    // 递归收集 text nodes，跳过 noisy 子树。
    walk_node(*el, buf);
}

fn walk_node(node: ego_tree::NodeRef<'_, scraper::Node>, buf: &mut String) {
    use scraper::Node;
    for child in node.children() {
        match child.value() {
            Node::Element(elem) => {
                let name = elem.name();
                if matches!(
                    name,
                    "script"
                        | "style"
                        | "noscript"
                        | "iframe"
                        | "nav"
                        | "header"
                        | "footer"
                        | "aside"
                ) {
                    continue;
                }
                walk_node(child, buf);
            }
            Node::Text(t) => {
                buf.push_str(t);
                buf.push(' ');
            }
            _ => {}
        }
    }
}

fn clean_whitespace(s: &str) -> String {
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
    fn extract_main_basic() {
        let html = r#"<html><head><title>T</title></head>
            <body><article>This is the article body. It is long enough to pass min length.
              And more text here for good measure: 中文内容也算字符长度的一部分用于计算正文长度。</article></body></html>"#;
        let (title, content) = extract_main(html);
        assert_eq!(title.as_deref(), Some("T"));
        assert!(content.as_deref().unwrap().contains("article body"));
    }

    /// Spec §5: article-stage failure 必须 code = article_extract_failed，
    /// reason 字段是分类字符串。
    #[test]
    fn failure_produces_article_extract_failed_with_reason() {
        let now = Utc::now();
        for r in [
            ArticleExtractReason::Network,
            ArticleExtractReason::Timeout,
            ArticleExtractReason::TooShort,
            ArticleExtractReason::UnsupportedContentType,
            ArticleExtractReason::HttpStatus,
            ArticleExtractReason::ParseError,
        ] {
            let out = failure("https://a.com/x", None, now, r, "boom");
            let err = out.error.expect("should have error");
            assert_eq!(err.code, ErrorCode::ArticleExtractFailed);
            assert_eq!(err.reason, r);
            // article cache 写入
            assert_eq!(out.article.url, "https://a.com/x");
            assert!(out.article.content.is_none());
            assert_eq!(out.article.warning, Some(WarningCode::ArticleMissing));
            // payload.reason 是字符串
            let payload_reason = out.article.payload.get("reason").and_then(|v| v.as_str());
            assert_eq!(payload_reason, Some(r.as_str()));
        }
    }

    #[test]
    fn reason_as_str_covers_all_variants() {
        assert_eq!(ArticleExtractReason::Network.as_str(), "network");
        assert_eq!(ArticleExtractReason::Timeout.as_str(), "timeout");
        assert_eq!(ArticleExtractReason::TooShort.as_str(), "too_short");
        assert_eq!(
            ArticleExtractReason::UnsupportedContentType.as_str(),
            "unsupported_content_type"
        );
        assert_eq!(ArticleExtractReason::HttpStatus.as_str(), "http_status");
        assert_eq!(ArticleExtractReason::ParseError.as_str(), "parse_error");
    }
}
