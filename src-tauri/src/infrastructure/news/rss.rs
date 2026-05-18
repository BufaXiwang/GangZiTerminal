//! RSS 资讯源——通用 RSS 2.0 解析，取前 60 条。
//!
//! 唯一 ID 推导：guid → link → 兜底 `{source}-{index}-{title}`。

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
        .enumerate()
        .map(|(index, item)| {
            let title = item.title().unwrap_or("未命名资讯").trim().to_string();
            let link = item.link().map(str::to_string);
            let published = item.pub_date().map(str::to_string);
            let summary = item
                .description()
                .map(strip_html)
                .filter(|value| !value.trim().is_empty());
            let id = item
                .guid()
                .map(|guid| guid.value().to_string())
                .or_else(|| link.clone())
                .unwrap_or_else(|| format!("{source}-{index}-{title}"));

            NewsItem {
                id,
                title,
                link,
                source: source.clone(),
                published,
                summary,
                analysis_status: None,
            }
        })
        .collect())
}
