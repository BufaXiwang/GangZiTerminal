//! News use case service — `fetch_news` / `list_news_sources` / `run_news_refresh` / `warm_articles`。
//!
//! Spec: docs/design/news-module.md §3 / §4 / §5
//!
//! 职责：把对外 DTO 翻译到 repository + provider 调用，按 spec 规则组装响应。
//! 完成后通过 `NewsRefreshedEvent` 给 adapters 层（adapters/news/events.rs）发布事件。
//!
//! 注：`run_news_refresh` 是 **内部 facade**（spec §4 内部 Rust API），由 scheduler 独占触发；
//! 不暴露为 Tauri command。

use crate::domain::news::errors::{WarmArticlesError, WarmArticlesErrorField};
use crate::domain::news::events::{
    NewsFailure, NewsRefreshStage, NewsRefreshWarning, NewsRefreshedPayload,
};
use crate::domain::news::source::NewsSource;
use crate::domain::news::types::{
    ArticleSnippet, FetchNewsError, FetchNewsItem, FetchNewsPage, FetchNewsRequest,
    FetchNewsResponse, ListNewsSourcesResponse, NewsItem, NewsItemFreshness, ProviderNewsItem,
    WarmArticlesRequest, WarmArticlesResponse, WarmArticlesResult,
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

/// News-refresh event sink；service 在 `warm_articles` 写入正文后用它发布 `news-refreshed`
/// 事件。具体 emit 实现由 adapters 层（lib.rs setup）注入，service 本身不依赖 Tauri。
pub type RefreshEventSink = Arc<dyn Fn(NewsRefreshedPayload) + Send + Sync + 'static>;

/// News BC 对外能力。包装 `AppDb` + `SourceRegistry` + providers，由 adapters / scheduler 持有。
pub struct NewsService {
    db: AppDb,
    registry: Arc<SourceRegistry>,
    rss: RssProvider,
    newsnow: NewsNowProvider,
    article: ArticleExtractor,
    /// adapters 层在 setup 时注入；scheduler 也是同一个 sink。
    /// `RwLock` 保证 `Arc<NewsService>` 持有者可在 setup 后回写 sink。
    event_sink: std::sync::RwLock<Option<RefreshEventSink>>,
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
const ARTICLE_EXCERPT_MAX_CHARS: usize = 500;
/// First-phase choice: warm "近期失败缓存" 抑制窗口写死 3600s；后续可参数化或
/// 挪入 references/news/article-extractor.md。
const WARM_RECENT_FAILURE_SUPPRESS_SECS: i64 = 3600;

/// 内部 refresh facade 的输入（不是 Tauri DTO）。spec §4 内部 Rust API。
#[derive(Debug, Clone, Default)]
pub struct RefreshBatchInput {
    pub sources: Option<Vec<String>>,
    #[allow(dead_code)]
    pub force: bool,
}

/// 内部 refresh facade 的输出。要么成功并产出 `NewsRefreshedPayload`，要么因为输入校验失败
/// 返回 `RefreshBatchError`。scheduler 只关心 payload；调试入口可以拿到错误细节。
#[derive(Debug, Clone)]
pub enum RefreshBatchOutcome {
    Ok(NewsRefreshedPayload),
    Err(RefreshBatchError),
}

#[derive(Debug, Clone)]
pub struct RefreshBatchError {
    pub code: ErrorCode,
    pub message: Option<String>,
}

impl NewsService {
    pub fn new(db: AppDb, registry: Arc<SourceRegistry>) -> reqwest::Result<Self> {
        Ok(Self {
            db,
            registry,
            rss: RssProvider::new()?,
            newsnow: NewsNowProvider::new()?,
            article: ArticleExtractor::new()?,
            event_sink: std::sync::RwLock::new(None),
        })
    }

    /// adapters 层在 setup 时注入事件发布回调（spec §5：warm 写正文后 emit `news-refreshed`）。
    pub fn set_event_sink(&self, sink: RefreshEventSink) {
        if let Ok(mut g) = self.event_sink.write() {
            *g = Some(sink);
        }
    }

    fn emit_news_refreshed(&self, payload: NewsRefreshedPayload) {
        if let Ok(g) = self.event_sink.read() {
            if let Some(sink) = g.as_ref() {
                sink(payload);
            }
        }
    }

    fn repo(&self) -> NewsRepository<'_> {
        NewsRepository::new(&self.db)
    }

    // ------------------------------------------------------------------ fetch
    // Spec: news-module.md §4 fetch_news（无 ids 字段）
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

        // 查询路径（spec §4：query / sources / 时间范围按 AND 组合）
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
    // Spec: news-module.md §4 内部 Rust API `run_news_refresh` / §5 provider 策略
    //
    // 这是 **内部 facade**，不暴露为 Tauri command（spec §4 明确禁止 refresh_news IPC）。
    // scheduler 独占触发；调试入口可以直接调用。
    pub async fn run_refresh(&self, input: RefreshBatchInput) -> RefreshBatchOutcome {
        // sources 校验（不存在则返回 invalid_input，不创建 batchId）
        if let Some(sources) = input.sources.as_ref() {
            for s in sources {
                if self.registry.get(s).is_none() {
                    return RefreshBatchOutcome::Err(RefreshBatchError {
                        code: ErrorCode::InvalidInput,
                        message: Some(format!("unknown source: {}", s)),
                    });
                }
            }
        }

        let batch_id = format!("news-refresh-{}", Uuid::new_v4());

        // 选择目标 sources
        let targets: Vec<_> = match input.sources.as_ref() {
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

        // First-phase: 顺序拉取，避免并发对同一 provider 叠加速率压力；spec §5 允许后续改并行。
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
            // Spec §5 line 372：fetchedCount 表示 provider 返回的原始 item 数量；
            // skippedCount 表示 normalize / validate 阶段跳过的 item 数量。
            // adapter 返回的 `items` 已经是 normalize 通过的子集，被跳过的写在 warns.skipped_count。
            let stage_skipped: u32 = warns.iter().filter_map(|w| w.skipped_count).sum();
            fetched_count += items.len() as u32 + stage_skipped;
            skipped_count += stage_skipped;
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

        RefreshBatchOutcome::Ok(payload)
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
            // 非 force：若已有成功正文，或近期失败缓存窗口内（first-phase: 写死 3600s；
            // 后续可参数化或挪入 references/news/article-extractor.md），则跳过。
            if !force {
                if let Ok(Some(existing)) = repo.get_article_content(&canonical_url) {
                    if existing.content.is_some()
                        || (now - existing.fetched_at).num_seconds()
                            < WARM_RECENT_FAILURE_SUPPRESS_SECS
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
            // Spec §5 article-stage failure: code 统一 article_extract_failed；
            // 细分原因写入 details.reason。
            if let Some(failure) = out.error {
                failures.push(NewsFailure {
                    provider: "article_extractor".to_string(),
                    source: None,
                    code: failure.code,
                    message: Some(failure.message),
                    details: Some(serde_json::json!({
                        "reason": failure.reason.as_str(),
                    })),
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

        let article_updated_news_ids: Vec<String> = updated_ids.into_iter().collect();
        let result = WarmArticlesResult {
            batch_id: batch_id.clone(),
            requested_count,
            attempted_count: attempted,
            article_updated_count,
            article_updated_news_ids: article_updated_news_ids.clone(),
            warnings: warnings.clone(),
            failures: failures.clone(),
        };

        // Spec §5: articleUpdatedCount > 0 时 emit `news-refreshed`，savedCount = 0，
        // articleUpdatedNewsIds 表达正文变化影响范围。
        if article_updated_count > 0 {
            let first_failure = failures.first().cloned();
            let failed_count = failures.len() as u32;
            let payload = NewsRefreshedPayload {
                batch_id: batch_id.clone(),
                fetched_count: 0,
                skipped_count: 0,
                saved_count: 0,
                article_updated_count,
                new_ids: vec![],
                updated_ids: vec![],
                article_updated_news_ids,
                failed_count,
                first_failure,
                failures,
                warnings,
            };
            self.emit_news_refreshed(payload);
        }

        WarmArticlesResponse::Ok(crate::domain::news::types::WarmArticlesOk {
            ok: Default::default(),
            result,
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

fn clamp_limit(req: Option<u32>, default: u32, max: u32) -> u32 {
    match req {
        None => default,
        Some(v) if v == 0 => default,
        Some(v) => v.min(max),
    }
}

/// 按 spec §4：取清洗后正文前 500 个字符（清洗后空白已折叠为单空格，不保留段落分隔）。
/// 按 char 计数避免切坏 utf-8。
fn build_excerpt(content: &str) -> String {
    let cleaned = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        return String::new();
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::db::run_migrations;
    use crate::infrastructure::news::migrations::migrations;
    use chrono::TimeZone;
    use std::sync::Mutex;

    /// Spec §4: articleExcerpt 取清洗后正文前 500 个字符（按 char 计数）。
    #[test]
    fn build_excerpt_takes_first_500_chars_after_whitespace_collapse() {
        let mut long = String::new();
        for _ in 0..600 {
            long.push('字');
        }
        let out = build_excerpt(&long);
        // 500 字 + 截断标记 '…'
        let count = out.chars().count();
        assert_eq!(count, 501);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn build_excerpt_collapses_whitespace_no_paragraph_split() {
        // 两段：原代码会取首段，新逻辑应该把两段折叠为单空格连接，整体取前 500。
        let input = "段一前部分\n\n段二后部分内容";
        let out = build_excerpt(input);
        assert!(out.contains("段一前部分"));
        assert!(out.contains("段二后部分内容"));
        assert!(!out.contains('\n'));
    }

    #[test]
    fn build_excerpt_handles_empty() {
        assert_eq!(build_excerpt(""), "");
        assert_eq!(build_excerpt("   \n  "), "");
    }

    fn setup_service() -> Arc<NewsService> {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, migrations()).unwrap());
        let registry = Arc::new(SourceRegistry::new());
        // bootstrap default sources
        {
            let repo = NewsRepository::new(&db);
            registry.bootstrap(&repo).unwrap();
        }
        Arc::new(NewsService::new(db, registry).unwrap())
    }

    fn insert_item(svc: &NewsService, id: &str, source: &str, title: &str, url: Option<&str>) {
        let it = NewsItem {
            id: id.to_string(),
            source: source.to_string(),
            title: title.to_string(),
            summary: Some("brief".to_string()),
            url: url.map(|s| s.to_string()),
            published_at: Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
            payload: serde_json::json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        svc.repo().upsert_news_item(&it).unwrap();
    }

    /// Spec §4: fetch_news({ query }) 走 FTS 相关性搜索。
    #[tokio::test]
    async fn fetch_news_query_runs_fts() {
        let svc = setup_service();
        insert_item(&svc, "id-a", "rss:sample", "GangZi quant terminal", None);
        insert_item(&svc, "id-b", "rss:sample", "completely different topic", None);
        let resp = svc.fetch_news(FetchNewsRequest {
            query: Some("gangzi".to_string()),
            ..Default::default()
        });
        assert_eq!(resp.items.len(), 1);
        assert_eq!(resp.items[0].id, "id-a");
        assert!(resp.errors.is_empty());
    }

    /// Spec §5: warm_articles articleUpdatedCount = 0 时不 emit `news-refreshed`。
    /// 这里给所有候选都不提供 URL，warm 路径走 warning + 不做抽取 + 不写正文。
    #[tokio::test]
    async fn warm_articles_no_url_does_not_emit() {
        let svc = setup_service();
        insert_item(&svc, "id-no-url", "rss:sample", "no url item", None);

        let emitted: Arc<Mutex<Vec<NewsRefreshedPayload>>> = Arc::new(Mutex::new(vec![]));
        let captured = Arc::clone(&emitted);
        svc.set_event_sink(Arc::new(move |p| {
            captured.lock().unwrap().push(p);
        }));

        let resp = svc
            .warm_articles(WarmArticlesRequest {
                news_ids: Some(vec!["id-no-url".to_string()]),
                ..Default::default()
            })
            .await;
        match resp {
            WarmArticlesResponse::Ok(ok) => {
                assert_eq!(ok.result.article_updated_count, 0);
                assert_eq!(ok.result.article_updated_news_ids.len(), 0);
                // 应该有一个 article_missing warning
                assert!(ok
                    .result
                    .warnings
                    .iter()
                    .any(|w| w.code == WarningCode::ArticleMissing));
            }
            WarmArticlesResponse::Err(e) => panic!("unexpected err: {:?}", e.error.code),
        }
        // emit 不应该被调用
        assert!(emitted.lock().unwrap().is_empty());
    }

    /// Spec §5: warm_articles articleUpdatedCount > 0 时必须 emit `news-refreshed`，
    /// savedCount = 0，articleUpdatedNewsIds 包含受影响新闻。
    ///
    /// 由于真实 article extractor 走 HTTP，这里通过直接调 repository 模拟"正文已经 warm 进来"
    /// 然后断言 emit 路径在 service 内已就位。本测试覆盖 emit-helper 本身的契约。
    #[test]
    fn emit_news_refreshed_invokes_sink() {
        let svc = setup_service();
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let c = Arc::clone(&counter);
        svc.set_event_sink(Arc::new(move |_p| {
            *c.lock().unwrap() += 1;
        }));
        let payload = NewsRefreshedPayload {
            batch_id: "test".to_string(),
            fetched_count: 0,
            skipped_count: 0,
            saved_count: 0,
            article_updated_count: 1,
            new_ids: vec![],
            updated_ids: vec![],
            article_updated_news_ids: vec!["id-a".to_string()],
            failed_count: 0,
            first_failure: None,
            failures: vec![],
            warnings: vec![],
        };
        svc.emit_news_refreshed(payload);
        assert_eq!(*counter.lock().unwrap(), 1);
    }

    /// Spec §5 failure code 表：article stage 失败 code 统一 article_extract_failed
    /// 且 details.reason 必填。本测试构造一个 ArticleExtractFailure（来自 article_extractor），
    /// 走 service warm 的 failure-push 分支，断言 NewsFailure 的字段。
    ///
    /// 由于 service 内联使用 article_extractor，模拟方式：直接把一个失败缓存 ArticleContent
    /// 写到 news_articles 表中，使非 force warm 跳过；然后用 force=true 触发抽取，
    /// 但 URL 是不可达的 example.invalid → extractor 返回 Network failure。
    #[tokio::test]
    async fn warm_articles_article_failure_uses_extract_failed_code() {
        let svc = setup_service();
        // 注意：本测试需要真实网络抽取失败。example.invalid TLD 在大多数 resolver 下立即失败。
        // 为了避免在 CI 上网络耗时，timeout=10s 内会返回。
        let url = "http://news-bc-test-example.invalid/article";
        insert_item(&svc, "id-fail", "rss:sample", "fail title", Some(url));
        let resp = svc
            .warm_articles(WarmArticlesRequest {
                news_ids: Some(vec!["id-fail".to_string()]),
                force: Some(true),
                ..Default::default()
            })
            .await;
        match resp {
            WarmArticlesResponse::Ok(ok) => {
                // 即使 0 个 article_updated，也应该至少有一个 failure
                assert!(!ok.result.failures.is_empty(), "expected failure");
                let f = &ok.result.failures[0];
                assert_eq!(f.code, ErrorCode::ArticleExtractFailed);
                assert_eq!(f.stage, Some(NewsRefreshStage::Article));
                // details.reason 必填
                let reason = f
                    .details
                    .as_ref()
                    .and_then(|d| d.get("reason"))
                    .and_then(|v| v.as_str());
                assert!(reason.is_some(), "details.reason must be present");
            }
            WarmArticlesResponse::Err(e) => panic!("unexpected err: {:?}", e.error.code),
        }
    }
}
