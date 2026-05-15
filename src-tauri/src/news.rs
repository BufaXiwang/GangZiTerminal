use crate::models::NewsItem;
use rss::Channel;
use serde_json::Value;
use std::io::Cursor;
use std::time::Duration;

pub async fn fetch_rss(url: String, source: String) -> Result<Vec<NewsItem>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("gangzi-terminal/0.1")
        .build()
        .map_err(|err| err.to_string())?;

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|err| format!("请求失败：{err}"))?;

    if !response.status().is_success() {
        return Err(format!("请求失败：HTTP {}", response.status()));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|err| format!("读取响应失败：{err}"))?;

    let channel =
        Channel::read_from(Cursor::new(bytes)).map_err(|err| format!("RSS解析失败：{err}"))?;

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
            }
        })
        .collect())
}

pub async fn fetch_newsnow_source(
    base_url: String,
    source_id: String,
    source_name: String,
) -> Result<Vec<NewsItem>, String> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("NewsNow 服务地址不能为空。".to_string());
    }

    let url = format!("{base}/api/s?id={source_id}&latest=true");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("Mozilla/5.0 gangzi-terminal/0.1")
        .build()
        .map_err(|err| err.to_string())?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| format!("NewsNow 请求失败：{err}"))?;

    if !response.status().is_success() {
        return Err(format!("NewsNow 请求失败：HTTP {}", response.status()));
    }

    let body = response
        .text()
        .await
        .map_err(|err| format!("NewsNow 响应读取失败：{err}"))?;
    let value: Value =
        serde_json::from_str(&body).map_err(|err| format!("NewsNow JSON解析失败：{err}"))?;
    let items = value
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| "NewsNow 响应缺少 items。".to_string())?;

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

fn strip_html(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_tag = false;

    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }

    output
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_string()
}
