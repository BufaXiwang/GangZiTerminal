//! 资讯刷新 use case——`run_news_refresh`。
//!
//! 流程：
//! 1. 遍历 default feed list（NewsNow source id 或 `rss:` 前缀的 RSS URL）
//! 2. 按 feed 串行 await（feed 数 < 10，无需并行）
//! 3. 去重 by id 合并到一个 Vec
//! 4. 写入 SQLite news_items
//! 5. emit `news-refreshed` 事件给前端 + Agent Runtime
//!
//! 失败处理（spec shared-types.md NewsFailure）：
//! - 每个失败映射为 `NewsFailure { provider, source, code: ErrorCode, stage, retryable }`
//! - 全失败 → 返 Err（取第一条失败原因）
//! - 部分失败 → 返 Ok 但 emit 带 failures[] + firstFailure + failedCount
//! - DB 写失败 → 返 Err（DB 错误优先级最高）

use crate::domain::news::NewsItem;
use crate::domain::shared::{
    ErrorCode, NewsFailure, NewsRefreshWarning, NewsRefreshedPayload, NewsStage, WarningCode,
};
use crate::infrastructure::news::{fetch_newsnow_source, fetch_rss};
use crate::pipeline::events::EVENT_AGENT_STATUS;
use serde_json::json;
use std::collections::HashSet;
use tauri::{AppHandle, Emitter};

const NEWSNOW_BASE_URL: &str = "https://newsnow.busiyi.world";
const NEWSNOW_PROVIDER: &str = "newsnow";
const RSS_PROVIDER: &str = "rss";

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

    fn provider(&self) -> &'static str {
        if self.target.starts_with("rss:") {
            RSS_PROVIDER
        } else {
            NEWSNOW_PROVIDER
        }
    }
}

pub async fn run_news_refresh(app: AppHandle) -> Result<NewsRefreshResult, String> {
    run_news_refresh_filtered(app, None).await
}

/// spec news-module.md §4 `RefreshNewsRequest.sources` —— 限定本次刷新使用的源。
/// `sources` 为 `None` 或空表示"全部默认源"；非空只刷新匹配名字 / target 的源。
pub async fn run_news_refresh_filtered(
    app: AppHandle,
    sources: Option<Vec<String>>,
) -> Result<NewsRefreshResult, String> {
    emit_status(&app, "refresh-news", "正在从 NewsNow 拉取资讯");

    let batch_id = format!("batch_{}", uuid::Uuid::new_v4().simple());
    let mut all_items: Vec<NewsItem> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut failures: Vec<NewsFailure> = Vec::new();
    let mut warnings: Vec<NewsRefreshWarning> = Vec::new();
    let mut skipped_count: usize = 0;

    let allow: Option<HashSet<String>> = sources
        .filter(|s| !s.is_empty())
        .map(|v| v.into_iter().collect());
    let feeds: Vec<Feed> = default_feeds()
        .into_iter()
        .filter(|f| match &allow {
            Some(set) => set.contains(f.name) || set.contains(f.target),
            None => true,
        })
        .collect();
    if feeds.is_empty() {
        return Err("invalid_input: 请求的 sources 在默认 feed 列表中未匹配".into());
    }

    for feed in feeds {
        let provider = feed.provider();
        let source_key = feed.target.to_string();
        let result = if let Some(rss_url) = feed.target.strip_prefix("rss:") {
            fetch_rss(rss_url.to_string(), feed.name.to_string()).await
        } else {
            fetch_newsnow_source(
                NEWSNOW_BASE_URL.to_string(),
                feed.target.to_string(),
                feed.name.to_string(),
            )
            .await
        };
        match result {
            Ok(items) => {
                let mut dropped_in_feed = 0usize;
                for item in items {
                    if seen_ids.insert(item.id.clone()) {
                        all_items.push(item);
                    } else {
                        dropped_in_feed += 1;
                    }
                }
                if dropped_in_feed > 0 {
                    skipped_count += dropped_in_feed;
                    warnings.push(NewsRefreshWarning {
                        provider: provider.into(),
                        source: Some(source_key.clone()),
                        code: WarningCode::DataPartial,
                        message: Some(format!("跨源去重丢弃 {dropped_in_feed} 条")),
                        stage: Some(NewsStage::Normalize),
                        skipped_count: Some(dropped_in_feed),
                        occurred_at: chrono::Utc::now().to_rfc3339(),
                    });
                }
            }
            Err(err) => {
                let retryable = err.is_retryable();
                let code = err.to_error_code();
                failures.push(NewsFailure {
                    provider: provider.into(),
                    source: Some(source_key),
                    code,
                    message: Some(err.to_string()),
                    details: None,
                    stage: Some(NewsStage::Fetch),
                    retryable: Some(retryable),
                    occurred_at: chrono::Utc::now().to_rfc3339(),
                });
            }
        }
    }

    let mut new_ids: Vec<String> = Vec::new();
    let mut updated_ids: Vec<String> = Vec::new();
    let mut save_failed = false;
    if !all_items.is_empty() {
        match crate::infrastructure::news::repository::save_news_items_with_diff(
            &app,
            all_items.clone(),
        ) {
            Ok(diff) => {
                new_ids = diff.new_ids;
                updated_ids = diff.updated_ids;
            }
            Err(err) => {
                save_failed = true;
                failures.push(NewsFailure {
                    provider: "repository".into(),
                    source: None,
                    code: ErrorCode::DbError,
                    message: Some(err.clone()),
                    details: None,
                    stage: Some(NewsStage::Save),
                    retryable: Some(false),
                    occurred_at: chrono::Utc::now().to_rfc3339(),
                });
            }
        }
        // 新入库的 news 由 agent_runtime 监听 news-refreshed 决定何时分析
    }

    // spec news-module.md §5：「savedCount = 0 且 articleUpdatedCount = 0 的 refresh
    // 不发布该事件；articleUpdatedCount > 0 时必须 emit」。
    let saved_count = new_ids.len() + updated_ids.len();
    let article_updated_count = 0usize; // warm_articles 接入后单独 emit
    let should_emit = saved_count > 0 || article_updated_count > 0;
    if should_emit {
        let payload = NewsRefreshedPayload {
            batch_id: batch_id.clone(),
            fetched_count: all_items.len(),
            skipped_count,
            saved_count,
            article_updated_count,
            new_ids: new_ids.clone(),
            updated_ids: updated_ids.clone(),
            article_updated_news_ids: None,
            failed_count: failures.len(),
            first_failure: failures.first().cloned(),
            failures: if failures.is_empty() {
                None
            } else {
                Some(failures.clone())
            },
            warnings: if warnings.is_empty() {
                None
            } else {
                Some(warnings.clone())
            },
        };
        let _ = app.emit("news-refreshed", &payload);
    }

    if all_items.is_empty() {
        return Err(failures
            .first()
            .map(|f| format!("{}: {}", f.provider, f.message.clone().unwrap_or_default()))
            .unwrap_or_else(|| "所有资讯源拉取失败".to_string()));
    }
    if save_failed {
        return Err(failures
            .iter()
            .find(|f| f.stage == Some(NewsStage::Save))
            .map(|f| format!("save_news_items: {}", f.message.clone().unwrap_or_default()))
            .unwrap_or_else(|| "save_news_items 失败".to_string()));
    }

    Ok(NewsRefreshResult {
        batch_id,
        fetched_count: all_items.len(),
        saved_count,
        failed_count: failures.len(),
        new_ids,
        updated_ids,
    })
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewsRefreshResult {
    pub batch_id: String,
    pub fetched_count: usize,
    pub saved_count: usize,
    pub failed_count: usize,
    pub new_ids: Vec<String>,
    pub updated_ids: Vec<String>,
}

fn emit_status(app: &AppHandle, phase: &str, message: &str) {
    let _ = app.emit(
        EVENT_AGENT_STATUS,
        json!({ "phase": phase, "message": message }),
    );
}
