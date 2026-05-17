//! 资讯刷新 use case——`run_news_refresh`。
//!
//! 流程：
//! 1. 遍历 default feed list（NewsNow source id 或 `rss:` 前缀的 RSS URL）
//! 2. 并行拉取（按 feed 串行 await，整体并不并行——足够，feed 数量 < 10）
//! 3. 去重 by id 合并到一个 Vec
//! 4. 写入 SQLite news_items
//! 5. emit `news-refreshed` 事件给前端
//!
//! 失败处理：单个 feed 失败累积到 `failures` 字符串列表，最终：
//! - 全失败 → 返 Err（第一条失败原因）
//! - 部分失败 → 返 Ok 但 emit 带 failedCount
//! - DB 写失败 → 返 Err（DB 错误优先级最高）

use crate::domain::news::NewsItem;
use crate::infrastructure::news::{fetch_newsnow_source, fetch_rss};
use crate::pipeline::EVENT_AGENT_STATUS;
use serde_json::{json, Value};
use std::collections::HashSet;
use tauri::{AppHandle, Emitter};

const NEWSNOW_BASE_URL: &str = "https://newsnow.busiyi.world";

/// 默认资讯源列表。
/// chinanews RSS 是独立源（不走 NewsNow 中转），NewsNow 单点故障时还能拉到东西。
fn default_feeds() -> Vec<Feed> {
    vec![
        Feed::news("wallstreetcn-quick", "华尔街见闻 快讯"),
        Feed::news("wallstreetcn-news", "华尔街见闻 最新"),
        Feed::news("cls-telegraph", "财联社 电报"),
        Feed::news("cls-depth", "财联社 深度"),
        Feed::news("gelonghui", "格隆汇 事件"),
        Feed::news("jin10", "金十数据"),
        Feed::news(
            "rss:https://www.chinanews.com.cn/rss/finance.xml",
            "中新网 财经",
        ),
    ]
}

struct Feed {
    name: &'static str,
    /// 如果以 `rss:` 开头视为 RSS 源，剩余部分是 URL；否则是 NewsNow source id
    target: &'static str,
}

impl Feed {
    const fn news(target: &'static str, name: &'static str) -> Self {
        Self { name, target }
    }
}

pub async fn run_news_refresh(app: AppHandle) -> Result<NewsRefreshResult, String> {
    emit_status(&app, "refresh-news", "正在从 NewsNow 拉取资讯");

    let mut all_items: Vec<NewsItem> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut failures: Vec<String> = Vec::new();

    for feed in default_feeds() {
        let result = if let Some(rss_url) = feed.target.strip_prefix("rss:") {
            fetch_rss(rss_url.to_string(), feed.name.to_string())
                .await
                .map_err(|e| e.to_string())
        } else {
            fetch_newsnow_source(
                NEWSNOW_BASE_URL.to_string(),
                feed.target.to_string(),
                feed.name.to_string(),
            )
            .await
            .map_err(|e| e.to_string())
        };
        match result {
            Ok(items) => {
                for item in items {
                    if seen_ids.insert(item.id.clone()) {
                        all_items.push(item);
                    }
                }
            }
            Err(err) => failures.push(format!("{}: {}", feed.name, err)),
        }
    }

    let payload: Vec<Value> = all_items
        .iter()
        .filter_map(|item| serde_json::to_value(item).ok())
        .collect();
    if !payload.is_empty() {
        if let Err(err) = crate::infrastructure::news::repository::save_news_items(app.clone(), payload) {
            failures.push(format!("save_news_items: {err}"));
        }
    }

    let _ = app.emit(
        "news-refreshed",
        json!({
            "fetchedCount": all_items.len(),
            "failedCount": failures.len(),
            "firstFailure": failures.first().cloned(),
        }),
    );

    if all_items.is_empty() {
        return Err(failures
            .first()
            .cloned()
            .unwrap_or_else(|| "所有资讯源拉取失败".to_string()));
    }
    if failures.iter().any(|m| m.starts_with("save_news_items:")) {
        return Err(failures
            .iter()
            .find(|m| m.starts_with("save_news_items:"))
            .cloned()
            .unwrap_or_else(|| "save_news_items 失败".to_string()));
    }

    Ok(NewsRefreshResult {
        fetched_count: all_items.len(),
        failed_count: failures.len(),
    })
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewsRefreshResult {
    pub fetched_count: usize,
    pub failed_count: usize,
}

fn emit_status(app: &AppHandle, phase: &str, message: &str) {
    let _ = app.emit(
        EVENT_AGENT_STATUS,
        json!({ "phase": phase, "message": message }),
    );
}
