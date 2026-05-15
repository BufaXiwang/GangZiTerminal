//! 资讯刷新入口。
//!
//! 注意：实时行情走 `pipeline::market_refresh`；
//! watchlist CRUD + 模拟账户 reset 已挪到 `adapters::account_commands`。
//! 本模块只保留 news 刷新。

use crate::db;
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

#[tauri::command]
pub async fn run_news_refresh(app: AppHandle) -> Result<NewsRefreshResult, String> {
    emit_status(&app, "refresh-news", "正在从 NewsNow 拉取资讯");

    let mut all_items: Vec<crate::models::NewsItem> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut failures: Vec<String> = Vec::new();

    for feed in default_feeds() {
        let result = if let Some(rss_url) = feed.target.strip_prefix("rss:") {
            crate::news::fetch_rss(rss_url.to_string(), feed.name.to_string()).await
        } else {
            crate::news::fetch_newsnow_source(
                NEWSNOW_BASE_URL.to_string(),
                feed.target.to_string(),
                feed.name.to_string(),
            )
            .await
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

    // 写入 SQLite——失败必须报告给调用方，不能吞掉返回 fetchedCount > 0 的假成功
    let payload: Vec<Value> = all_items
        .iter()
        .filter_map(|item| serde_json::to_value(item).ok())
        .collect();
    if !payload.is_empty() {
        if let Err(err) = db::save_news_items(app.clone(), payload) {
            failures.push(format!("save_news_items: {err}"));
        }
    }

    let pending = db::count_pending_news(app.clone()).unwrap_or(0);
    let _ = app.emit(
        "news-refreshed",
        json!({
            "fetchedCount": all_items.len(),
            "pendingCount": pending,
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
        pending_count: pending,
        failed_count: failures.len(),
    })
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewsRefreshResult {
    pub fetched_count: usize,
    pub pending_count: i64,
    pub failed_count: usize,
}

fn emit_status(app: &AppHandle, phase: &str, message: &str) {
    let _ = app.emit(
        EVENT_AGENT_STATUS,
        json!({ "phase": phase, "message": message }),
    );
}
