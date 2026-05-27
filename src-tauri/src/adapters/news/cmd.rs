//! News Tauri commands — `fetch_news` / `list_news_sources` / `warm_articles`。
//!
//! Spec: docs/design/news-module.md §4 / §5
//!
//! 注意：spec §4 已删除手动 `refresh_news` IPC 命令；refresh 由 scheduler 独占触发，
//! 调试入口使用内部 facade `NewsService::run_refresh`。
//!
//! `warm_articles` 走 `{ ok: true | false }` 形状的响应，把失败语义编码在 payload 里；
//! 因此命令函数返回 `Result<T, CommandError>` 时只会在"调用方传参完全无法解析 / 内部 panic"
//! 这种意外路径才进 `Err`。正常 invalid_input 走 `ok = false`。

use crate::adapters::error::CommandError;
use crate::domain::news::types::{
    FetchNewsRequest, FetchNewsResponse, ListNewsSourcesResponse, WarmArticlesRequest,
    WarmArticlesResponse,
};
use crate::pipeline::news::NewsService;
use std::sync::Arc;
use tauri::State;

/// 同步读取 News 本地读模型。
///
/// Spec: news-module.md §4 fetch_news
#[tauri::command]
#[specta::specta]
pub fn fetch_news(
    request: FetchNewsRequest,
    service: State<'_, Arc<NewsService>>,
) -> Result<FetchNewsResponse, CommandError> {
    Ok(service.fetch_news(request))
}

/// 列出当前已知 source。
///
/// Spec: news-module.md §4 list_news_sources
#[tauri::command]
#[specta::specta]
pub fn list_news_sources(
    service: State<'_, Arc<NewsService>>,
) -> Result<ListNewsSourcesResponse, CommandError> {
    Ok(service.list_news_sources())
}

/// 显式 warm articles。
///
/// Spec: news-module.md §5 warm_articles
#[tauri::command]
#[specta::specta]
pub async fn warm_articles(
    request: WarmArticlesRequest,
    service: State<'_, Arc<NewsService>>,
) -> Result<WarmArticlesResponse, CommandError> {
    let svc = service.inner().clone();
    Ok(svc.warm_articles(request).await)
}
