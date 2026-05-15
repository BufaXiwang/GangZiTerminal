mod cleanup;
mod extractors;
mod metadata;
mod model;

use crate::article::extractors::extract_article;
use crate::article::metadata::{extract_author, extract_images, extract_published, extract_title};
use crate::article::model::ExtractContext;
use crate::db;
use crate::models::ArticleContent;
use crate::security::{validate_external_url, MAX_RESPONSE_BYTES};
use scraper::Html;
use std::time::Duration;
use tauri::AppHandle;

/// 拉文章正文。逻辑顺序：缓存命中 → 直接返；否则 fetch + 保存到 article_contents。
/// 缓存查询和写入都在后端做——前端只 invoke 一次拿结果，不再单独触发 save。
///
/// `item_id` 可选：传了就把 fetch 结果写入 article_contents 缓存表（前端通常会传，
/// scheduler / 内部任务可以省略）。
#[tauri::command]
pub async fn fetch_article_content(
    app: AppHandle,
    url: String,
    item_id: Option<String>,
    source: Option<String>,
    fallback_title: Option<String>,
    fallback_summary: Option<String>,
    fallback_published: Option<String>,
) -> Result<ArticleContent, String> {
    // 缓存命中且有正文 → 直接返
    if let Ok(Some(cached_value)) = db::load_article_content(app.clone(), url.clone()) {
        if let Ok(cached) = serde_json::from_value::<ArticleContent>(cached_value) {
            if !cached.paragraphs.is_empty() {
                return Ok(cached);
            }
        }
    }

    let article = fetch_article_remote(
        url,
        source,
        fallback_title,
        fallback_summary,
        fallback_published,
    )
    .await?;

    // 落盘缓存——失败静默（缓存丢失不影响调用方）
    if let Ok(value) = serde_json::to_value(&article) {
        let _ = db::save_article_content(app, item_id, value);
    }
    Ok(article)
}

async fn fetch_article_remote(
    url: String,
    source: Option<String>,
    fallback_title: Option<String>,
    fallback_summary: Option<String>,
    fallback_published: Option<String>,
) -> Result<ArticleContent, String> {
    // SSRF 守门：拒绝 javascript:/data:/file:、localhost、内网、链路本地等
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

    // 响应过大直接拒绝（基于 Content-Length）
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
