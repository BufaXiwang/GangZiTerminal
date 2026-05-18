//! NewsNow 中转源——从 https://newsnow.busiyi.world/api/s 拉一批资讯。
//!
//! 一个 source_id 对应一条 feed（华尔街见闻、财联社、格隆汇、金十数据 等）。
//! id 加 source_id 前缀避免不同 feed 撞 id。

use crate::domain::news::{NewsError, NewsItem};
use crate::infrastructure::news::util::strip_html;
use serde_json::Value;
use std::time::Duration;

pub async fn fetch_newsnow_source(
    base_url: String,
    source_id: String,
    source_name: String,
) -> Result<Vec<NewsItem>, NewsError> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(NewsError::Config("NewsNow 服务地址不能为空".into()));
    }

    let url = format!("{base}/api/s?id={source_id}&latest=true");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("Mozilla/5.0 gangzi-terminal/0.1")
        .build()
        .map_err(|err| NewsError::Network(err.to_string()))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| NewsError::Network(format!("NewsNow 请求失败：{err}")))?;

    if !response.status().is_success() {
        return Err(NewsError::Network(format!(
            "NewsNow 请求失败：HTTP {}",
            response.status()
        )));
    }

    let body = response
        .text()
        .await
        .map_err(|err| NewsError::Network(format!("NewsNow 响应读取失败：{err}")))?;
    let value: Value = serde_json::from_str(&body)
        .map_err(|err| NewsError::Decode(format!("NewsNow JSON 解析失败：{err}")))?;
    let items = value
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| NewsError::Decode("NewsNow 响应缺少 items".into()))?;

    Ok(items
        .iter()
        .take(60)
        .enumerate()
        .map(|(index, item)| {
            let title = string_field(item, "title");
            let link = item
                .get("url")
                .and_then(Value::as_str)
                .or_else(|| item.get("mobileUrl").and_then(Value::as_str))
                .map(str::to_string);
            let published = newsnow_pub_date(item);
            let summary = item
                .pointer("/extra/hover")
                .and_then(Value::as_str)
                .map(strip_html)
                .filter(|value| !value.trim().is_empty());
            let id = item
                .get("id")
                .and_then(|value| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .or_else(|| value.as_i64().map(|num| num.to_string()))
                })
                .or_else(|| link.clone())
                .unwrap_or_else(|| format!("{source_id}-{index}-{title}"));

            NewsItem {
                id: format!("{source_id}-{id}"),
                title,
                link,
                source: source_name.clone(),
                published,
                summary,
                analysis_status: None,
            }
        })
        .collect())
}

fn string_field(item: &Value, key: &str) -> String {
    item.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn newsnow_pub_date(item: &Value) -> Option<String> {
    item.get("pubDate")
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_string)
                .or_else(|| value.as_i64().map(|num| num.to_string()))
        })
        .or_else(|| {
            item.pointer("/extra/date").and_then(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| value.as_i64().map(|num| num.to_string()))
            })
        })
}
