//! Article 正文抽取——fetch HTML + 多 extractor 投票 + metadata 抽取。
//!
//! 模块组成：
//! - `cleanup`: HTML 文本清洗 / 噪声词过滤 / dedupe
//! - `extractors`: 各家站点的正文 selector（chinanews / cls / gelonghui 等）
//! - `metadata`: 标题 / 作者 / 发表时间 / 配图抽取
//! - `model`: ExtractContext / 内部 struct
//!
//! 仅暴露 `fetch_article_remote`（pure fetch + parse），缓存读写由调用方（adapter / pipeline）负责。

pub mod cleanup;
pub mod extractors;
pub mod metadata;
pub mod model;

use crate::domain::news::ArticleContent;
use crate::infrastructure::news::article::extractors::extract_article;
use crate::infrastructure::news::article::metadata::{
    extract_author, extract_images, extract_published, extract_title,
};
use crate::infrastructure::news::article::model::ExtractContext;
use crate::infrastructure::security::{validate_external_url, MAX_RESPONSE_BYTES};
use scraper::Html;
use std::time::Duration;

/// 拉一篇资讯的正文 + metadata。
///
/// 行为：
/// 1. URL 校验（SSRF / scheme / 内网拒绝）
/// 2. HTTP GET（带 UA 头、超时 16s、Content-Length 上限）
/// 3. parse HTML → extractor 投票 + metadata 抽取
/// 4. 兜底：抽不到正文时用 fallback_summary，再不行用占位文案
///
/// 不读不写 DB——缓存策略由 adapter / pipeline 决定。
pub async fn fetch_article_remote(
    url: String,
    source: Option<String>,
    fallback_title: Option<String>,
    fallback_summary: Option<String>,
    fallback_published: Option<String>,
) -> Result<ArticleContent, String> {
    validate_external_url(&url)?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(16))
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36")
        .build()
        .map_err(|err| err.to_string())?;

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|err| format!("原文请求失败：{err}"))?;

    if let Some(len) = response.content_length() {
        if len > MAX_RESPONSE_BYTES {
            return Err(format!(
                "原文响应过大：{len} bytes（上限 {MAX_RESPONSE_BYTES}）"
            ));
        }
    }

    let html = response
        .text()
        .await
        .map_err(|err| format!("原文读取失败：{err}"))?;
    if html.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(format!(
            "原文实际大小 {} 超过上限 {MAX_RESPONSE_BYTES}",
            html.len()
        ));
    }

    let document = Html::parse_document(&html);
    let context = ExtractContext {
        url: &url,
        source: source.as_deref(),
        fallback_title: fallback_title.as_deref(),
        fallback_summary: fallback_summary.as_deref(),
    };
    let extracted = extract_article(&document, &context);
    let title = choose_title(extract_title(&document), fallback_title, source.as_deref());
    let paragraphs = if extracted.paragraphs.is_empty() {
        fallback_summary
            .map(|summary| vec![summary])
            .unwrap_or_else(|| vec!["暂时无法提取正文，请点击右上角打开原文。".to_string()])
    } else {
        extracted.paragraphs
    };

    Ok(ArticleContent {
        url: url.clone(),
        title,
        source,
        published: extract_published(&document).or(fallback_published),
        author: extract_author(&document),
        paragraphs,
        images: extract_images(&document, &url),
        fetched_at: chrono::Utc::now().to_rfc3339(),
        extraction: extracted.extractor,
    })
}

fn choose_title(
    extracted_title: Option<String>,
    fallback_title: Option<String>,
    source: Option<&str>,
) -> String {
    match (extracted_title, fallback_title) {
        (Some(extracted), Some(fallback)) if title_is_site_name(&extracted, source) => fallback,
        (Some(extracted), Some(fallback))
            if extracted.chars().count() < 8 && fallback.chars().count() >= 8 =>
        {
            fallback
        }
        (Some(extracted), _) if !title_is_site_name(&extracted, source) => extracted,
        (_, Some(fallback)) => fallback,
        _ => "未命名资讯".to_string(),
    }
}

fn title_is_site_name(title: &str, source: Option<&str>) -> bool {
    let compact_title = title.replace(' ', "");
    let compact_source = source.unwrap_or_default().replace(' ', "");
    compact_title == compact_source
        || [
            "华尔街见闻",
            "财联社",
            "金十数据",
            "格隆汇",
            "雪球",
            "中新经纬",
        ]
        .iter()
        .any(|site| compact_title == *site)
}
