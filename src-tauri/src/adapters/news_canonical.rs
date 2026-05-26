#![allow(dead_code)] // 请求结构持有 spec 全字段，部分参数（如 RefreshNewsRequest.sources）尚未消费

//! News canonical Tauri commands —— spec `news-module.md §4`。
//!
//! 四个入口：
//! - `fetch_news` —— canonical 富查询（ids / query / sources / publishedFrom/To /
//!   includeArticle / limit / offset）；走 `repository::query_news_items` + FTS rank +
//!   articleExcerpt 派生
//! - `list_news_sources` —— 扫 news_items 已出现的 source 集合
//! - `refresh_news_canonical` —— 透传 `pipeline::news::run_news_refresh`
//! - `warm_articles` —— spec discriminated union；按 newsIds / recentLimit 选候选 →
//!   `fetch_article_remote` → `save_article_content`；articleUpdatedNewsIds 含共享
//!   canonical URL 的兄弟 NewsItem；articleUpdatedCount > 0 时 emit `news-refreshed`

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::infrastructure::db::{migrate, open_database};

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FetchNewsRequest {
    #[serde(default)]
    pub ids: Option<Vec<String>>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub sources: Option<Vec<String>>,
    #[serde(default)]
    pub published_from: Option<String>,
    #[serde(default)]
    pub published_to: Option<String>,
    #[serde(default)]
    pub include_article: bool,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchNewsArticle {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchNewsItem {
    pub id: String,
    pub source: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub article_excerpt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub article: Option<FetchNewsArticle>,
    /// spec news-module.md §4：FetchNewsItem.warnings 严格 WarningCode 闭集合。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<crate::domain::shared::WarningCode>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchNewsErrorEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// spec news-module.md §4 errors[].code: ErrorCode（闭集合）
    pub code: crate::domain::shared::ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    pub limit: i64,
    pub offset: i64,
    pub has_more: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchNewsResponse {
    pub items: Vec<FetchNewsItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<FetchNewsErrorEntry>,
    pub page: PageInfo,
}

#[tauri::command]
pub fn fetch_news(
    app: AppHandle,
    request: Option<FetchNewsRequest>,
) -> Result<FetchNewsResponse, String> {
    let req = request.unwrap_or_default();
    let limit = req.limit.unwrap_or(50).clamp(1, 200);
    let offset = req.offset.unwrap_or(0).max(0);
    let include_article = req.include_article;

    let opts = crate::infrastructure::news::repository::NewsQueryOpts {
        ids: req.ids.clone(),
        query: req.query.clone(),
        sources: req.sources.clone(),
        published_from: req.published_from.clone(),
        published_to: req.published_to.clone(),
        limit: Some(limit + 1), // +1 探测 has_more
        offset: Some(offset),
    };
    let mut rows = crate::infrastructure::news::repository::query_news_items(&app, opts)?;
    let has_more = rows.len() as i64 > limit;
    if has_more {
        rows.truncate(limit as usize);
    }

    let mut errors: Vec<FetchNewsErrorEntry> = Vec::new();
    // ids 模式：未命中走 errors
    if let Some(ids) = &req.ids {
        let found: std::collections::HashSet<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        for id in ids {
            if !found.contains(id.as_str()) {
                errors.push(FetchNewsErrorEntry {
                    id: Some(id.clone()),
                    code: crate::domain::shared::ErrorCode::NotFound,
                    message: None,
                });
            }
        }
    }

    let items: Vec<FetchNewsItem> = rows
        .iter()
        .map(|r| to_canonical_item(&app, r, include_article))
        .collect();
    Ok(FetchNewsResponse {
        items,
        errors,
        page: PageInfo {
            limit,
            offset,
            has_more,
        },
    })
}

use crate::domain::news::{clean_excerpt, ARTICLE_EXCERPT_MAX_CHARS};

fn to_canonical_item(
    app: &AppHandle,
    r: &crate::domain::news::NewsItem,
    include_article: bool,
) -> FetchNewsItem {
    use crate::domain::news::canonical_url::canonical_url;
    use crate::domain::shared::WarningCode;
    let mut warnings: Vec<WarningCode> = Vec::new();
    let mut article_excerpt: Option<String> = None;
    let mut article: Option<FetchNewsArticle> = None;

    if let Some(url) = r.link.as_deref().filter(|s| !s.is_empty()) {
        let canonical = canonical_url(url);
        if let Ok(Some(payload)) =
            crate::infrastructure::news::repository::load_article_content_ref(app, &canonical)
        {
            let content = payload
                .get("content")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(String::from);
            if let Some(c) = content.clone() {
                // spec §4「articleExcerpt 当本地有正文时**必须**由 News query facade 生成并返回」
                article_excerpt = Some(clean_excerpt(&c, ARTICLE_EXCERPT_MAX_CHARS));
                if include_article {
                    article = Some(FetchNewsArticle {
                        title: payload
                            .get("title")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        content: c,
                        fetched_at: payload
                            .get("fetchedAt")
                            .and_then(|v| v.as_str())
                            .or_else(|| payload.get("fetched_at").and_then(|v| v.as_str()))
                            .map(String::from),
                    });
                }
            } else if include_article {
                warnings.push(WarningCode::ArticleMissing);
            }
        } else if include_article {
            warnings.push(WarningCode::ArticleMissing);
        }
    } else if include_article {
        warnings.push(WarningCode::ArticleMissing);
    }

    FetchNewsItem {
        id: r.id.clone(),
        source: r.source.clone(),
        title: r.title.clone(),
        summary: r.summary.clone(),
        url: r.link.clone(),
        published_at: r.published.clone(),
        article_excerpt,
        article,
        warnings,
    }
}

// clean_excerpt 抽到 domain::news::excerpt 公共 helper（spec §4 摘要规则）。

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewsSourceEntry {
    pub source_id: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListNewsSourcesResponse {
    pub items: Vec<NewsSourceEntry>,
}

/// 列出当前已观察到的 source 集合（按 news_items 表实际出现过的 source 推断）。
/// TODO: 拆出独立 news_sources 配置表后改为权威读模型。
#[tauri::command]
pub fn list_news_sources(app: AppHandle) -> Result<ListNewsSourcesResponse, String> {
    let conn = open_database(&app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare("select distinct source from news_items order by source")
        .map_err(|e| format!("prepare 失败：{e}"))?;
    let mut rows = stmt.query([]).map_err(|e| format!("query 失败：{e}"))?;
    let mut items = Vec::new();
    while let Some(r) = rows.next().map_err(|e| format!("next 失败：{e}"))? {
        let source_id: String = r.get(0).map_err(|e| e.to_string())?;
        let provider = source_id
            .split(':')
            .next()
            .unwrap_or(&source_id)
            .to_string();
        items.push(NewsSourceEntry {
            source_id,
            provider,
            display_name: None,
            enabled: true,
        });
    }
    Ok(ListNewsSourcesResponse { items })
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RefreshNewsRequest {
    #[serde(default)]
    pub sources: Option<Vec<String>>,
    #[serde(default)]
    pub force: bool,
}

/// spec news-module.md §307 `RefreshNewsError`：错误码 + 可选 field 提示。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshNewsError {
    pub code: crate::domain::shared::ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// spec news-module.md §307 `RefreshNewsResponse` discriminated union：
/// `{ ok: true, result: NewsRefreshedPayload }` | `{ ok: false, error: RefreshNewsError }`。
/// `#[serde(untagged)]` 让 Variant 自动按字段集分发到对应分支。
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum RefreshNewsResponse {
    Ok {
        ok: bool, // 始终 true
        result: crate::domain::shared::NewsRefreshedPayload,
    },
    Err {
        ok: bool, // 始终 false
        error: RefreshNewsError,
    },
}

/// canonical refresh 入口。spec news-module.md §4：`sources` 显式列出时只刷新匹配源；
/// 未知 source / 非法 force 走 `RefreshNewsResponse::Err` 不创建 batchId。
#[tauri::command]
pub async fn refresh_news_canonical(
    app: AppHandle,
    request: Option<RefreshNewsRequest>,
) -> Result<RefreshNewsResponse, String> {
    let sources = request.and_then(|r| r.sources);
    match crate::pipeline::news::run_news_refresh_filtered(app, sources).await {
        Ok(result) => Ok(RefreshNewsResponse::Ok {
            ok: true,
            result: crate::domain::shared::NewsRefreshedPayload {
                batch_id: result.batch_id,
                fetched_count: result.fetched_count,
                skipped_count: 0,
                saved_count: result.saved_count,
                article_updated_count: 0,
                new_ids: result.new_ids,
                updated_ids: result.updated_ids,
                article_updated_news_ids: None,
                failed_count: result.failed_count,
                first_failure: None,
                failures: None,
                warnings: None,
            },
        }),
        Err(msg) => {
            // spec §307: unknown source / 非法 force / provider 全失败 → ErrorCode 映射
            let code = if msg.contains("invalid_input") {
                crate::domain::shared::ErrorCode::InvalidInput
            } else {
                crate::domain::shared::ErrorCode::ProviderUnavailable
            };
            Ok(RefreshNewsResponse::Err {
                ok: false,
                error: RefreshNewsError {
                    code,
                    field: if msg.contains("sources") {
                        Some("sources".into())
                    } else {
                        None
                    },
                    message: Some(msg),
                },
            })
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WarmArticlesRequest {
    #[serde(default)]
    pub news_ids: Option<Vec<String>>,
    #[serde(default)]
    pub recent_limit: Option<i64>,
    #[serde(default)]
    pub force: bool,
}

/// spec news-module.md §4 WarmArticlesResult。warnings / failures 使用 shared
/// 类型化结构（spec shared-types.md §6 `NewsFailure` / `NewsRefreshWarning`），
/// 避免 untyped JSON 漂移。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WarmArticlesResult {
    pub batch_id: String,
    pub requested_count: u32,
    pub attempted_count: u32,
    pub article_updated_count: u32,
    pub article_updated_news_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<crate::domain::shared::NewsRefreshWarning>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<crate::domain::shared::NewsFailure>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WarmArticlesError {
    pub code: crate::domain::shared::ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// spec news-module.md §4 WarmArticlesResponse 是 discriminated union。
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum WarmArticlesResponse {
    Ok {
        ok: bool, // 始终 true
        result: WarmArticlesResult,
    },
    Err {
        ok: bool, // 始终 false
        error: WarmArticlesError,
    },
}

impl WarmArticlesResponse {
    pub fn ok(result: WarmArticlesResult) -> Self {
        WarmArticlesResponse::Ok { ok: true, result }
    }
    pub fn err(code: crate::domain::shared::ErrorCode, message: impl Into<String>) -> Self {
        WarmArticlesResponse::Err {
            ok: false,
            error: WarmArticlesError {
                code,
                field: None,
                message: Some(message.into()),
            },
        }
    }
}

/// spec news-module.md §4 `warm_articles`：按 newsIds 或 recentLimit 选候选 →
/// `fetch_article_remote` → `save_article_content`；返回 discriminated union。
/// articleUpdatedCount > 0 时 emit `news-refreshed`（spec §5）。
/// articleUpdatedNewsIds 包含共享 canonical URL 的所有兄弟 NewsItem.id（spec L461）。
#[tauri::command]
pub async fn warm_articles(
    app: AppHandle,
    request: Option<WarmArticlesRequest>,
) -> Result<WarmArticlesResponse, String> {
    use crate::domain::news::canonical_url::canonical_url;
    use crate::domain::shared::ErrorCode;
    use crate::infrastructure::news::article::fetch_article_remote;
    use crate::infrastructure::news::repository as nrepo;
    use tauri::Emitter;

    let req = request.unwrap_or_default();

    // 1. 输入校验 + 候选集
    let (candidates, missing_ids): (Vec<crate::domain::news::NewsItem>, Vec<String>) =
        if let Some(ids) = req.news_ids.as_ref() {
            if ids.len() > 200 {
                return Ok(WarmArticlesResponse::err(
                    ErrorCode::InvalidInput,
                    "newsIds 最多 200 个",
                ));
            }
            let rows = nrepo::get_news_items_by_ids(app.clone(), ids.clone())
                .map_err(|e| e)?;
            let found: std::collections::HashSet<&str> =
                rows.iter().map(|r| r.id.as_str()).collect();
            let missing: Vec<String> = ids
                .iter()
                .filter(|id| !found.contains(id.as_str()))
                .cloned()
                .collect();
            (rows, missing)
        } else {
            let recent_limit = req.recent_limit.unwrap_or(50).clamp(1, 200);
            let rows = nrepo::list_news_items(app.clone(), Some(recent_limit))
                .map_err(|e| e)?;
            (rows, Vec::new())
        };

    // spec §4 L459：未找到的 newsIds → 不创建 batchId，返回 ok=false / not_found
    if !missing_ids.is_empty() {
        return Ok(WarmArticlesResponse::err(
            ErrorCode::NotFound,
            format!(
                "未找到 newsIds: {}",
                missing_ids.join(",")
            ),
        ));
    }

    let batch_id = format!("warm_{}", uuid::Uuid::new_v4().simple());
    let requested_count = candidates.len() as u32;

    // 同一 canonical URL 可能对应多条 NewsItem（不同 source 同文）；
    // 预扫一遍 NewsItem 反查表：canonical_url -> Vec<news_id>
    let url_to_news_ids: std::collections::HashMap<String, Vec<String>> = {
        let mut map: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        // 用 list_news_items 一次拿最近 N 条做反查（性能折中，避免每条都 query）。
        // 候选很多时退化为只用 candidates 自身集合反查。
        for item in &candidates {
            if let Some(u) = item.link.as_deref().filter(|s| !s.is_empty()) {
                let c = canonical_url(u);
                if !c.is_empty() {
                    map.entry(c).or_default().push(item.id.clone());
                }
            }
        }
        map
    };

    // 2. 逐条 fetch + save
    use crate::domain::shared::{NewsFailure, NewsRefreshWarning, NewsStage, WarningCode as WC};
    use crate::domain::shared::ErrorCode as EC;
    let mut attempted = 0u32;
    let mut updated_news_ids: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let mut failures: Vec<NewsFailure> = Vec::new();
    let mut warnings: Vec<NewsRefreshWarning> = Vec::new();
    for item in &candidates {
        let Some(url) = item.link.as_deref().filter(|s| !s.is_empty()) else {
            warnings.push(NewsRefreshWarning {
                provider: "article_extractor".into(),
                source: Some(item.source.clone()),
                code: WC::DataPartial,
                message: Some(format!("news {} 无 link，跳过", item.id)),
                stage: Some(NewsStage::Article),
                skipped_count: Some(1),
                occurred_at: chrono::Utc::now().to_rfc3339(),
            });
            continue;
        };
        let canon = canonical_url(url);
        if canon.is_empty() {
            warnings.push(NewsRefreshWarning {
                provider: "article_extractor".into(),
                source: Some(item.source.clone()),
                code: WC::DataPartial,
                message: Some(format!("news {} canonical_url 为空", item.id)),
                stage: Some(NewsStage::Normalize),
                skipped_count: Some(1),
                occurred_at: chrono::Utc::now().to_rfc3339(),
            });
            continue;
        }
        // force=false 时跳过已有 content
        if !req.force {
            if let Ok(Some(payload)) = nrepo::load_article_content_ref(&app, &canon) {
                if payload
                    .get("content")
                    .and_then(|v| v.as_str())
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false)
                {
                    continue;
                }
            }
        }
        attempted += 1;
        match fetch_article_remote(
            canon.clone(),
            Some(item.source.clone()),
            Some(item.title.clone()),
            item.summary.clone(),
            item.published.clone(),
        )
        .await
        {
            Ok(article) => {
                let payload = serde_json::json!({
                    "url": canon,
                    "title": article.title,
                    "content": article.content,
                    "fetchedAt": article.fetched_at,
                });
                if nrepo::save_article_content(app.clone(), Some(item.id.clone()), payload).is_ok() {
                    // spec L461：articleUpdatedNewsIds 必须含共享同一 canonical URL 的所有 NewsItem
                    if let Some(siblings) = url_to_news_ids.get(&canon) {
                        for sid in siblings {
                            updated_news_ids.insert(sid.clone());
                        }
                    } else {
                        updated_news_ids.insert(item.id.clone());
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    target = "news.warm_articles",
                    news_id = %item.id,
                    url = %canon,
                    error = %e,
                    "正文抽取失败"
                );
                failures.push(NewsFailure {
                    provider: "article_extractor".into(),
                    source: Some(item.source.clone()),
                    code: EC::ArticleExtractFailed,
                    message: Some(e),
                    details: Some(serde_json::json!({ "newsId": item.id })),
                    stage: Some(NewsStage::Article),
                    retryable: Some(true),
                    occurred_at: chrono::Utc::now().to_rfc3339(),
                });
            }
        }
    }

    let updated_vec: Vec<String> = updated_news_ids.into_iter().collect();
    let article_updated_count = updated_vec.len() as u32;
    // spec §5：articleUpdatedCount > 0 必须 emit news-refreshed
    if article_updated_count > 0 {
        let _ = app.emit(
            "news-refreshed",
            serde_json::json!({
                "batchId": batch_id.clone(),
                "fetchedCount": 0,          // warm 路径不拉新闻列表
                "skippedCount": 0,
                "savedCount": 0,
                "articleUpdatedCount": article_updated_count,
                "newIds": [],
                "updatedIds": [],
                "articleUpdatedNewsIds": updated_vec,
                "failedCount": failures.len(),
            }),
        );
    }

    Ok(WarmArticlesResponse::ok(WarmArticlesResult {
        batch_id,
        requested_count,
        attempted_count: attempted,
        article_updated_count,
        article_updated_news_ids: updated_vec,
        warnings,
        failures,
    }))
}
