//! RSS 资讯源——通用 RSS 2.0 解析，取前 60 条。
//!
//! 唯一 ID 推导：guid → link → 兜底 `{source}-{index}-{title}`。

use crate::domain::news::canonical_url::{
    canonical_url, fingerprint_news_id, stable_news_id,
};
use crate::domain::news::{NewsError, NewsItem};
use crate::infrastructure::news::util::strip_html;
use rss::Channel;
use std::io::Cursor;
use std::time::Duration;

pub async fn fetch_rss(url: String, source: String) -> Result<Vec<NewsItem>, NewsError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("gangzi-terminal/0.1")
        .build()
        .map_err(|err| NewsError::Network(err.to_string()))?;

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|err| NewsError::Network(format!("请求失败：{err}")))?;

    if !response.status().is_success() {
        return Err(NewsError::Network(format!(
            "请求失败：HTTP {}",
            response.status()
        )));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|err| NewsError::Network(format!("读取响应失败：{err}")))?;

    let channel = Channel::read_from(Cursor::new(bytes))
        .map_err(|err| NewsError::Decode(format!("RSS 解析失败：{err}")))?;

    Ok(channel
        .items()
        .iter()
        .take(60)
        .filter_map(|item| {
            let title = item.title().unwrap_or("未命名资讯").trim().to_string();
            // spec news-module.md §2：link 走 canonical URL 再算稳定 ID
            let raw_link = item.link().map(str::to_string);
            let canonical_link = raw_link
                .as_deref()
                .map(canonical_url)
                .filter(|s| !s.is_empty());
            let published = item.pub_date().map(str::to_string);
            let summary = item
                .description()
                .map(strip_html)
                .filter(|value| !value.trim().is_empty());
            let raw_guid = item.guid().map(|g| g.value().to_string());
            // 稳定 ID 规则：source:url:sha256 → source:item:sha256 →
            // fingerprint(title|published|summary) → 全空跳过
            let id = match stable_news_id(&source, canonical_link.as_deref(), raw_guid.as_deref())
            {
                Ok(id) => id,
                Err(_) => {
                    let summary_norm = summary.as_deref().map(|s| s.trim());
                    match fingerprint_news_id(
                        &source,
                        Some(title.trim()).filter(|s| !s.is_empty()),
                        published.as_deref(),
                        summary_norm,
                    ) {
                        Ok(id) => id,
                        Err(_) => {
                            tracing::warn!(
                                target = "news.rss",
                                source = %source,
                                title = %title,
                                "RSS item 缺 link / guid / 可指纹字段，跳过（spec §2）"
                            );
                            return None;
                        }
                    }
                }
            };

            let now = chrono::Utc::now().to_rfc3339();
            // payload 保留 provider 原始信息（含 raw URL）便于审计
            let payload = serde_json::json!({
                "originalUrl": raw_link,
                "rawGuid": raw_guid,
            });
            Some(NewsItem {
                id,
                title,
                link: canonical_link,
                source: source.clone(),
                published,
                summary,
                payload,
                created_at: now.clone(),
                updated_at: now,
            })
        })
        .collect())
}
