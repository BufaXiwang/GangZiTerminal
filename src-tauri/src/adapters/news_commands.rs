//! News Tauri IPC commands——前端入口。
//!
//! 两条命令：
//! - `run_news_refresh`：手动触发刷新，委托给 `pipeline::news::run_news_refresh`
//! - `fetch_article_content`：拉一篇正文（先查 article_contents 缓存，未命中调
//!   `infrastructure::news::article::fetch_article_remote` 然后写缓存）
//!
//! `list_news_items` / `get_news_items_by_ids` 仍直接挂在 `db` 模块上（纯 DB 查询，
//! 没必要再包一层 adapter）。

use crate::domain::news::ArticleContent;
use crate::infrastructure::news::article::fetch_article_remote;
use crate::pipeline::news::{self, NewsRefreshResult};
use tauri::AppHandle;

#[tauri::command]
pub async fn run_news_refresh(app: AppHandle) -> Result<NewsRefreshResult, String> {
    news::run_news_refresh(app).await
}

/// 拉文章正文——逻辑：缓存命中 → 直接返；否则 fetch + 保存。
/// 缓存查询和写入都在后端，前端只 invoke 一次拿结果。
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
    if let Ok(Some(cached_value)) = crate::infrastructure::news::repository::load_article_content(app.clone(), url.clone()) {
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

    if let Ok(value) = serde_json::to_value(&article) {
        let _ = crate::infrastructure::news::repository::save_article_content(app, item_id, value);
    }
    Ok(article)
}
