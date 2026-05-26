//! News use case service — `fetch_news` / `list_news_sources` / `refresh_news` / `warm_articles`。
//!
//! Spec: docs/design/news-module.md §3 / §4 / §5
//!
//! 职责：把对外 DTO 翻译到 repository + provider 调用，按 spec 规则组装响应。
//! 完成后通过 `NewsRefreshedEvent` 给 adapters 层（adapters/news/events.rs）发布事件。

use crate::domain::news::errors::{
    RefreshNewsError, RefreshNewsErrorField, WarmArticlesError, WarmArticlesErrorField,
};
use crate::domain::news::events::{
    NewsFailure, NewsRefreshStage, NewsRefreshWarning, NewsRefreshedPayload,
};
use crate::domain::news::source::NewsSource;
use crate::domain::news::types::{
    ArticleSnippet, FetchNewsError, FetchNewsItem, FetchNewsPage, FetchNewsRequest,
    FetchNewsResponse, ListNewsSourcesResponse, NewsItem, NewsItemFreshness, ProviderNewsItem,
    RefreshNewsErr, RefreshNewsOk, RefreshNewsRequest, RefreshNewsResponse, WarmArticlesRequest,
    WarmArticlesResponse, WarmArticlesResult,
};
use crate::domain::shared::{ErrorCode, WarningCode};
use crate::infrastructure::db::AppDb;
use crate::infrastructure::news::article_extractor::ArticleExtractor;
use crate::infrastructure::news::newsnow::NewsNowProvider;
use crate::infrastructure::news::registry::SourceRegistry;
use crate::infrastructure::news::repository::{
    NewsRepository, RepoArticleUpsertOutcome, RepoItemUpsertOutcome,
};
use crate::infrastructure::news::rss::RssProvider;
use chrono::Utc;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

/// News BC 对外能力。包装 `AppDb` + `SourceRegistry` + providers，由 adapters / scheduler 持有。
pub struct NewsService {
    db: AppDb,
    registry: Arc<SourceRegistry>,
    rss: RssProvider,
    newsnow: NewsNowProvider,
    article: ArticleExtractor,
}

/// 给 adapters 层 emit 用的事件信息。
#[derive(Debug, Clone)]
pub struct NewsRefreshedEvent {
    pub payload: NewsRefreshedPayload,
}

const FETCH_LIMIT_DEFAULT: u32 = 50;
const FETCH_LIMIT_MAX: u32 = 200;
const WARM_RECENT_DEFAULT: u32 = 50;
const WARM_RECENT_MAX: u32 = 200;
const WARM_IDS_MAX: usize = 200;
const FETCH_IDS_MAX: usize = 200;
const ARTICLE_EXCERPT_MAX_CHARS: usize = 500;

impl NewsService {
    pub fn new(db: AppDb, registry: Arc<SourceRegistry>) -> reqwest::Result<Self> {
        Ok(Self {
            db,
            registry,
            rss: RssProvider::new()?,
            newsnow: NewsNowProvider::new()?,
            article: ArticleExtractor::new()?,
        })
    }

    fn repo(&self) -> NewsRepository<'_> {
        NewsRepository::new(&self.db)
    }

    // ------------------------------------------------------------------ fetch
    // Spec: news-module.md §4 fetch_news
    pub fn fetch_news(&self, req: FetchNewsRequest) -> FetchNewsResponse {
        let limit = clamp_limit(req.limit, FETCH_LIMIT_DEFAULT, FETCH_LIMIT_MAX);
        let offset = req.offset.unwrap_or(0);
        let include_article = req.include_article.unwrap_or(false);

        // sources validation
        let mut response_errors: Vec<FetchNewsError> = Vec::new();
        if let Some(srcs) = req.sources.as_ref() {
            for s in srcs {
                if !self.registry.contains(s) {
                    response_errors.push(FetchNewsError {
                        id: None,
                        field: Some("sources".to_string()),
                        code: ErrorCode::InvalidInput,
                        message: Some(format!("unknown source: {}", s)),
                    });
                }
            }
            if !response_errors.is_empty() {
                return FetchNewsResponse {
                    items: vec![],
                    errors: response_errors,
                    page: FetchNewsPage {
                        limit,
                        offset,
                        has_more: false,
                    },
                };
            }
        }

        // ids 路径优先
        if let Some(ids) = req.ids.as_ref() {
            if ids.len() > FETCH_IDS_MAX {
                return FetchNewsResponse {
                    items: vec![],
                    errors: vec![FetchNewsError {
                        id: None,
                        field: Some("ids".to_string()),
                        code: ErrorCode::InvalidInput,
                        message: Some(format!("ids exceed max {}", FETCH_IDS_MAX)),
                    }],
                    page: FetchNewsPage {
                        limit,
                        offset,
                        has_more: false,
                    },
                };
            }
            return self.fetch_by_ids(ids, &req, limit, offset, include_article);
        }

        // 一般查询路径
        let repo = self.repo();
        let result = match repo.list_news_items(
            req.sources.as_deref(),
            req.published_from.as_ref(),
            req.published_to.as_ref(),
            req.query.as_deref(),
            limit,
            offset,
        ) {
            Ok(r) => r,
            Err(e) => {
                return FetchNewsResponse {
                    items: vec![],
                    errors: vec![FetchNewsError {
                        id: None,
                        field: None,
                        code: ErrorCode::DbError,
                        message: Some(e.to_string()),
                    }],
                    page: FetchNewsPage {
                        limit,
                        offset,
                        has_more: false,
                    },
                };
            }
        };

        let now = Utc::now();
        let items = result
            .items
            .into_iter()
            .map(|it| self.build_fetch_item(&repo, it, include_article, now))
            .collect::<Vec<_>>();

        let has_more = (offset as u64 + items.len() as u64) < result.total as u64;
        FetchNewsResponse {
            items,
            errors: response_errors,
            page: FetchNewsPage {
                limit,
                offset,
                has_more,
            },
        }
    }

    fn fetch_by_ids(
        &self,
        ids: &[String],
        req: &FetchNewsRequest,
        limit: u32,
        offset: u32,
        include_article: bool,
    ) -> FetchNewsResponse {
        let repo = self.repo();
        let now = Utc::now();
        let raw = match repo.get_news_items_by_ids(ids) {
            Ok(v) => v,
            Err(e) => {
                return FetchNewsResponse {
                    items: vec![],
                    errors: vec![FetchNewsError {
                        id: None,
                        field: None,
                        code: ErrorCode::DbError,
                        message: Some(e.to_string()),
                    }],
                    page: FetchNewsPage {
                        limit,
                        offset,
                        has_more: false,
                    },
                };
            }
        };

        let mut items: Vec<FetchNewsItem> = Vec::new();
        let mut errors: Vec<FetchNewsError> = Vec::new();
        // 保持输入顺序；缺失的 ID 进 errors（spec §4）
        for (id, found) in ids.iter().zip(raw.into_iter()) {
            match found {
                Some(item) => {
                    // 应用 sources / time / query 过滤（spec §4：先 ids 再其他过滤）
                    if !passes_other_filters(&item, req) {
                        continue;
                    }
                    items.push(self.build_fetch_item(&repo, item, include_article, now));
                }
                None => errors.push(FetchNewsError {
                    id: Some(id.clone()),
                    field: None,
                    code: ErrorCode::NotFound,
                    message: None,
                }),
            }
        }

        // 再应用 limit/offset（spec §4 ids + 分页）
        let total = items.len() as u32;
        let paged: Vec<_> = items
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();
        let has_more = (offset as u64 + paged.len() as u64) < total as u64;
        FetchNewsResponse {
            items: paged,
            errors,
            page: FetchNewsPage {
                limit,
                offset,
                has_more,
            },
        }
    }

    fn build_fetch_item(
        &self,
        repo: &NewsRepository<'_>,
        item: NewsItem,
        include_article: bool,
        now: chrono::DateTime<Utc>,
    ) -> FetchNewsItem {
        let mut warnings: Vec<WarningCode> = Vec::new();
        let mut article_snippet: Option<ArticleSnippet> = None;
        let mut excerpt: Option<String> = None;
        let mut article_fetched_at = None;

        if let Some(url) = item.url.as_deref() {
            if let Ok(Some(art)) = repo.get_article_content(url) {
                article_fetched_at = Some(art.fetched_at);
                if let Some(content) = art.content.as_deref() {
                    if !content.is_empty() {
                        excerpt = Some(build_excerpt(content));
                        if include_article {
                            article_snippet = Some(ArticleSnippet {
                                title: art.title.clone(),
                                content: content.to_string(),
                                fetched_at: Some(art.fetched_at),
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
        } else if include_article {
            warnings.push(WarningCode::ArticleMissing);
        }

        let age_ms = (now - item.created_at).num_milliseconds();
        let freshness = Some(NewsItemFreshness {
            age_ms: Some(age_ms),
            article_fetched_at,
        });

        FetchNewsItem {
            id: item.id,
            source: item.source,
            title: item.title,
            summary: item.summary,
            url: item.url,
            published_at: item.published_at,
            article_excerpt: excerpt,
            article: article_snippet,
            freshness,
            warnings,
            errors: vec![],
        }
    }

    // ------------------------------------------------------------------ sources
    pub fn list_news_sources(&self) -> ListNewsSourcesResponse {
        let repo = self.repo();
        let items = repo.list_sources().unwrap_or_default();
        ListNewsSourcesResponse { items }
    }

    // ------------------------------------------------------------------ refresh
    // Spec: news-module.md §4 refresh_news / §5 provider 策略
    pub async fn refresh_news(&self, req: RefreshNewsRequest) -> RefreshNewsResponse {
        // sources 校验（spec §4：包含未知/禁用/非法时返回 invalid_input，不创建 batchId）
        if let Some(sources) = req.sources.as_ref() {
            for s in sources {
                match self.registry.get(s) {
                    None => {
                        return RefreshNewsResponse::Err(RefreshNewsErr {
                            ok: Default::default(),
                            error: RefreshNewsError {
                                code: ErrorCode::InvalidInput,
                                field: Some(RefreshNewsErrorField::Sources),
                                message: Some(format!("unknown source: {}", s)),
                            },
                        });
                    }
                    Some(src) if !src.enabled => {
                        return RefreshNewsResponse::Err(RefreshNewsErr {
                            ok: Default::default(),
                            error: RefreshNewsError {
                                code: ErrorCode::InvalidInput,
                                field: Some(RefreshNewsErrorField::Sources),
                                message: Some(format!("source disabled: {}", s)),
                            },
                        });
                    }
                    _ => {}
                }
            }
        }

        let batch_id = format!("news-refresh-{}", Uuid::new_v4());

        // 选择目标 sources
        let targets: Vec<_> = match req.sources.as_ref() {
            Some(s) => s
                .iter()
                .filter_map(|sid| self.registry.get(sid))
                .collect(),
            None => self.registry.enabled(),
        };

        let mut fetched_count: u32 = 0;
        let mut skipped_count: u32 = 0;
        let mut warnings: Vec<NewsRefreshWarning> = Vec::new();
        let mut failures: Vec<NewsFailure> = Vec::new();
        let mut new_ids: Vec<String> = Vec::new();
        let mut updated_ids: Vec<String> = Vec::new();

        // 顺序拉取（spec §5：单个 provider 失败不影响其他 source）；目前不并发，简单实现。
        for src in targets {
            let (items, mut warns, failure) = match src.provider.as_str() {
                "rss" => self.rss.fetch(&src).await,
                "newsnow" => self.newsnow.fetch(&src).await,
                other => (
                    vec![],
                    vec![],
                    Some(NewsFailure {
                        provider: other.to_string(),
                        source: Some(src.source_id.clone()),
                        code: ErrorCode::InvalidInput,
                        message: Some(format!("unknown provider: {}", other)),
                        details: None,
                        stage: Some(NewsRefreshStage::Normalize),
                        retryable: Some(false),
                        occurred_at: Utc::now(),
                    }),
                ),
            };
            fetched_count += items.len() as u32;
            skipped_count += warns
                .iter()
                .filter_map(|w| w.skipped_count)
                .sum::<u32>();
            warnings.append(&mut warns);
            if let Some(f) = failure {
                let when = f.occurred_at;
                let code = f.code;
                let msg = f.message.clone();
                failures.push(f);
                let _ = self.repo().record_source_refresh_err(
                    &src.source_id,
                    code,
                    msg.as_deref(),
                    when,
                );
                continue;
            }

            // save NewsItem
            let now = Utc::now();
            let news_items: Vec<NewsItem> = items
                .into_iter()
                .map(|p| provider_to_news_item(p, now))
                .collect();

            match self.repo().upsert_news_items_batch(&news_items) {
                Ok(outcomes) => {
                    for (id, oc) in outcomes {
                        match oc {
                            RepoItemUpsertOutcome::Inserted => new_ids.push(id),
                            RepoItemUpsertOutcome::Updated => updated_ids.push(id),
                            RepoItemUpsertOutcome::Unchanged => {}
                        }
                    }
                    let _ = self.repo().record_source_refresh_ok(&src.source_id, now);
                }
                Err(e) => {
                    let f = NewsFailure {
                        provider: src.provider.clone(),
                        source: Some(src.source_id.clone()),
                        code: ErrorCode::DbError,
                        message: Some(e.to_string()),
                        details: None,
                        stage: Some(NewsRefreshStage::Save),
                        retryable: Some(true),
                        occurred_at: Utc::now(),
                    };
                    failures.push(f);
                }
            }
        }

        let saved_count = (new_ids.len() + updated_ids.len()) as u32;
        let first_failure = failures.first().cloned();
        let failed_count = failures.len() as u32;

        let payload = NewsRefreshedPayload {
            batch_id: batch_id.clone(),
            fetched_count,
            skipped_count,
            saved_count,
            article_updated_count: 0,
            new_ids,
            updated_ids,
            article_updated_news_ids: vec![],
            failed_count,
            first_failure,
            failures,
            warnings,
        };

        RefreshNewsResponse::Ok(RefreshNewsOk {
            ok: Default::default(),
            result: payload,
        })
    }

    // ------------------------------------------------------------------ warm_articles
    // Spec: news-module.md §5
    pub async fn warm_articles(&self, req: WarmArticlesRequest) -> WarmArticlesResponse {
        // 输入校验（spec §5）
        if let Some(ids) = req.news_ids.as_ref() {
            if ids.len() > WARM_IDS_MAX {
                return WarmArticlesResponse::Err(crate::domain::news::types::WarmArticlesErr {
                    ok: Default::default(),
                    error: WarmArticlesError {
                        code: ErrorCode::InvalidInput,
                        field: Some(WarmArticlesErrorField::NewsIds),
                        message: Some(format!("newsIds exceed max {}", WARM_IDS_MAX)),
                    },
                });
            }
        }
        let recent_limit = clamp_limit(req.recent_limit, WARM_RECENT_DEFAULT, WARM_RECENT_MAX);
        let force = req.force.unwrap_or(false);

        let repo = self.repo();
        let now = Utc::now();

        // 选候选 NewsItem
        let candidates: Vec<NewsItem> = match req.news_ids.as_ref() {
            Some(ids) => {
                let raw = match repo.get_news_items_by_ids(ids) {
                    Ok(v) => v,
                    Err(e) => {
                        return WarmArticlesResponse::Err(
                            crate::domain::news::types::WarmArticlesErr {
                                ok: Default::default(),
                                error: WarmArticlesError {
                                    code: ErrorCode::DbError,
                                    field: None,
                                    message: Some(e.to_string()),
                                },
                            },
                        );
                    }
                };
                // 任一缺失 → not_found，不创建 batchId
                let mut found = Vec::with_capacity(ids.len());
                for (id, item) in ids.iter().zip(raw.into_iter()) {
                    match item {
                        Some(i) => found.push(i),
                        None => {
                            return WarmArticlesResponse::Err(
                                crate::domain::news::types::WarmArticlesErr {
                                    ok: Default::default(),
                                    error: WarmArticlesError {
                                        code: ErrorCode::NotFound,
                                        field: Some(WarmArticlesErrorField::NewsIds),
                                        message: Some(format!("news not found: {}", id)),
                                    },
                                },
                            );
                        }
                    }
                }
                found
            }
            None => repo.select_recent_for_warm(recent_limit).unwrap_or_default(),
        };

        let batch_id = format!("news-warm-{}", Uuid::new_v4());
        let mut warnings: Vec<NewsRefreshWarning> = Vec::new();
        let mut failures: Vec<NewsFailure> = Vec::new();
        let mut attempted: u32 = 0;
        let mut article_updated_count: u32 = 0;
        let mut updated_ids: HashSet<String> = HashSet::new();

        // 按 canonical URL 去重避免一个批次内多次抓同一 URL
        let mut url_to_first_news: HashMap<String, String> = HashMap::new();
        for it in &candidates {
            if let Some(u) = it.url.as_deref() {
                url_to_first_news
                    .entry(u.to_string())
                    .or_insert_with(|| it.id.clone());
            } else {
                warnings.push(NewsRefreshWarning {
                    provider: "article_extractor".to_string(),
                    source: Some(it.source.clone()),
                    code: WarningCode::ArticleMissing,
                    message: Some(format!("news {} has no url", it.id)),
                    stage: Some(NewsRefreshStage::Article),
                    skipped_count: Some(1),
                    occurred_at: now,
                });
            }
        }

        for (canonical_url, first_news_id) in url_to_first_news {
            // 非 force：若已有成功正文 / 失败缓存（fetched_at 不久前）则跳过
            if !force {
                if let Ok(Some(existing)) = repo.get_article_content(&canonical_url) {
                    if existing.content.is_some()
                        || (now - existing.fetched_at).num_seconds() < 3600
                    {
                        continue;
                    }
                }
            }

            attempted += 1;
            let out = self.article.extract(&canonical_url, Some(&first_news_id)).await;

            // 即使失败也写缓存（spec §抑制短期重试）
            match repo.upsert_article_content(&out.article) {
                Ok(RepoArticleUpsertOutcome::Updated { affected_news_ids }) => {
                    if out.article.content.is_some() {
                        article_updated_count += 1;
                    }
                    for id in affected_news_ids {
                        updated_ids.insert(id);
                    }
                }
                Ok(RepoArticleUpsertOutcome::Unchanged) => {}
                Err(e) => {
                    failures.push(NewsFailure {
                        provider: "article_extractor".to_string(),
                        source: None,
                        code: ErrorCode::DbError,
                        message: Some(e.to_string()),
                        details: None,
                        stage: Some(NewsRefreshStage::Save),
                        retryable: Some(true),
                        occurred_at: now,
                    });
                }
            }
            if let Some((code, msg)) = out.error {
                failures.push(NewsFailure {
                    provider: "article_extractor".to_string(),
                    source: None,
                    code,
                    message: Some(msg),
                    details: None,
                    stage: Some(NewsRefreshStage::Article),
                    retryable: Some(true),
                    occurred_at: now,
                });
            } else if out.article.warning == Some(WarningCode::ArticleMissing)
                && out.article.content.is_none()
            {
                warnings.push(NewsRefreshWarning {
                    provider: "article_extractor".to_string(),
                    source: None,
                    code: WarningCode::ArticleMissing,
                    message: out.article.payload.get("reason").and_then(|v| v.as_str()).map(String::from),
                    stage: Some(NewsRefreshStage::Article),
                    skipped_count: None,
                    occurred_at: now,
                });
            }
        }

        let requested_count = req
            .news_ids
            .as_ref()
            .map(|v| v.len() as u32)
            .unwrap_or(candidates.len() as u32);

        WarmArticlesResponse::Ok(crate::domain::news::types::WarmArticlesOk {
            ok: Default::default(),
            result: WarmArticlesResult {
                batch_id,
                requested_count,
                attempted_count: attempted,
                article_updated_count,
                article_updated_news_ids: updated_ids.into_iter().collect(),
                warnings,
                failures,
            },
        })
    }
}

fn provider_to_news_item(p: ProviderNewsItem, now: chrono::DateTime<Utc>) -> NewsItem {
    NewsItem {
        id: p.id,
        source: p.source,
        title: p.title,
        summary: p.summary,
        url: p.url,
        published_at: p.published_at,
        payload: p.payload,
        created_at: now,
        updated_at: now,
    }
}

fn passes_other_filters(item: &NewsItem, req: &FetchNewsRequest) -> bool {
    if let Some(sources) = req.sources.as_ref() {
        if !sources.iter().any(|s| s == &item.source) {
            return false;
        }
    }
    if let Some(from) = req.published_from.as_ref() {
        match item.published_at.as_ref() {
            None => return false,
            Some(pa) if pa < from => return false,
            _ => {}
        }
    }
    if let Some(to) = req.published_to.as_ref() {
        match item.published_at.as_ref() {
            None => return false,
            Some(pa) if pa > to => return false,
            _ => {}
        }
    }
    // query 不在 ids 路径应用：spec §4 没有要求；但我们仍然按"先 ids 再其他过滤"应用以保守。
    if let Some(q) = req.query.as_ref() {
        let q = q.to_lowercase();
        let hit = item.title.to_lowercase().contains(&q)
            || item
                .summary
                .as_deref()
                .map(|s| s.to_lowercase().contains(&q))
                .unwrap_or(false);
        if !hit {
            return false;
        }
    }
    true
}

fn clamp_limit(req: Option<u32>, default: u32, max: u32) -> u32 {
    match req {
        None => default,
        Some(v) if v == 0 => default,
        Some(v) => v.min(max),
    }
}

fn build_excerpt(content: &str) -> String {
    // 取清洗后首段；超过 max chars 截断（按 char 计数避免切坏 utf-8）
    let normalized = content.trim();
    if normalized.is_empty() {
        return String::new();
    }
    let first_block = normalized
        .split("\n\n")
        .next()
        .unwrap_or(normalized);
    let cleaned = first_block.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = cleaned.chars().take(ARTICLE_EXCERPT_MAX_CHARS).collect();
    if out.chars().count() < cleaned.chars().count() {
        out.push('…');
    }
    out
}

/// 为对外 `NewsSource` 列表过滤一下（暂未使用，保留以便 adapters 复用）。
#[allow(dead_code)]
pub fn filter_sources_by_enabled(sources: Vec<NewsSource>, only_enabled: bool) -> Vec<NewsSource> {
    if !only_enabled {
        return sources;
    }
    sources.into_iter().filter(|s| s.enabled).collect()
}
