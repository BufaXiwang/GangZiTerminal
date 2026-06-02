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
    FetchNewsResponse, ListNewsSourcesResponse, NewsDateCount, NewsItem, NewsItemFreshness,
    ProviderNewsItem, WarmArticlesRequest, WarmArticlesResponse, WarmArticlesResult,
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
                    date_counts: vec![],
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
                    date_counts: vec![],
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

        // 每日真实条数（同 filter，不分页）给日期导航。失败不致命，空表退化。
        let date_counts = repo
            .count_news_by_date(
                req.sources.as_deref(),
                req.published_from.as_ref(),
                req.published_to.as_ref(),
                req.query.as_deref(),
            )
            .unwrap_or_default()
            .into_iter()
            .map(|(date, count)| NewsDateCount { date, count })
            .collect();

        FetchNewsResponse {
            items,
            errors: response_errors,
            page: FetchNewsPage {
                limit,
                offset,
                has_more,
            },
            date_counts,
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
        // 新入库且有 URL 的 item，刷新时同步抓正文（按 source 策略）：(source, news_id, url)。
        let mut to_fetch_body: Vec<(String, String, String)> = Vec::new();

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
                            RepoItemUpsertOutcome::Inserted => {
                                // 新 item 有 URL → 排队抓正文（按 source 策略，刷新时同步抓）。
                                if let Some(u) = news_items
                                    .iter()
                                    .find(|it| it.id == id)
                                    .and_then(|it| it.url.clone())
                                {
                                    to_fetch_body.push((src.source_id.clone(), id.clone(), u));
                                }
                                new_ids.push(id);
                            }
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

        // 刷新时同步抓正文（按 source 策略，并发≤6）。抓到 → 存 ArticleContent；
        // 快讯(TitleIsContent) 跳过；抓不到 → 记 debug log，不入正文、不算 item 失败。
        let mut article_updated_news_ids: Vec<String> = Vec::new();
        if !to_fetch_body.is_empty() {
            use futures_util::StreamExt;
            let mut stream = futures_util::stream::iter(to_fetch_body)
                .map(|(source, news_id, url)| {
                    let extractor = &self.article;
                    async move {
                        let out = extractor.extract_for_source(&source, &url, Some(&news_id)).await;
                        (source, news_id, url, out)
                    }
                })
                .buffer_unordered(6);
            while let Some((source, news_id, url, out)) = stream.next().await {
                match out {
                    None => {} // TitleIsContent：标题即全文，跳过
                    Some(o) => {
                        if o.error.is_none() && o.article.content.is_some() {
                            if self.repo().upsert_article_content(&o.article).is_ok() {
                                article_updated_news_ids.push(news_id);
                            }
                        } else {
                            let reason = o
                                .error
                                .as_ref()
                                .map(|e| e.reason.as_str())
                                .unwrap_or("empty");
                            tracing::debug!(
                                target: "news.article",
                                source = %source, url = %url, reason,
                                "article body fetch missed"
                            );
                        }
                    }
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
            article_updated_count: article_updated_news_ids.len() as u32,
            new_ids,
            updated_ids,
            article_updated_news_ids,
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
                    // Spec §5: 仅在"成功写入或更新 ArticleContent"时计入 articleUpdatedCount
                    // 与 articleUpdatedNewsIds——失败缓存（content=None）首次写入不算成功。
                    // 两个字段必须 lockstep 同进同退。
                    if out.article.content.is_some() {
                        article_updated_count += 1;
                        for id in affected_news_ids {
                            updated_ids.insert(id);
                        }
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
    use crate::domain::news::source::NewsSourceRef;
    use chrono::{DateTime, TimeZone};
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

    // ======================================================================
    // 以下为 spec-driven 补齐：fetch_news 读取契约（pagination / ordering /
    // date_counts / time-range / includeArticle / sources 校验）+ list_news_sources
    // + warm_articles 输入校验。命名前缀 `news_spec_*`。
    // ======================================================================

    /// helper：插一条带定制 published_at / created_at 的 item。
    fn insert_item_full(
        svc: &NewsService,
        id: &str,
        source: &str,
        title: &str,
        url: Option<&str>,
        published_at: Option<DateTime<Utc>>,
        created_at: DateTime<Utc>,
    ) {
        let it = NewsItem {
            id: id.to_string(),
            source: source.to_string(),
            title: title.to_string(),
            summary: None,
            url: url.map(|s| s.to_string()),
            published_at,
            payload: serde_json::json!({}),
            created_at,
            updated_at: created_at,
        };
        svc.repo().upsert_news_item(&it).unwrap();
    }

    /// Spec §4: limit 默认 50、最大 200，offset 默认 0；hasMore 基于同一查询条件。
    #[test]
    fn news_spec_fetch_limit_clamps_and_defaults() {
        let svc = setup_service();
        // limit 默认 50
        let r = svc.fetch_news(FetchNewsRequest::default());
        assert_eq!(r.page.limit, 50, "default limit must be 50");
        assert_eq!(r.page.offset, 0, "default offset must be 0");
        // limit 超过 200 被 clamp 到 200
        let r = svc.fetch_news(FetchNewsRequest {
            limit: Some(9999),
            ..Default::default()
        });
        assert_eq!(r.page.limit, 200, "limit must clamp to max 200");
        // limit = 0 视为默认
        let r = svc.fetch_news(FetchNewsRequest {
            limit: Some(0),
            ..Default::default()
        });
        assert_eq!(r.page.limit, 50, "limit=0 falls back to default 50");
    }

    /// Spec §4: hasMore 基于同一查询的总数（不被分页限制误判）。
    #[test]
    fn news_spec_fetch_pagination_has_more() {
        let svc = setup_service();
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        for i in 0..5 {
            insert_item_full(
                &svc,
                &format!("id-{i}"),
                "rss:sample",
                &format!("title {i}"),
                None,
                Some(base + chrono::Duration::seconds(i)),
                base + chrono::Duration::seconds(i),
            );
        }
        // limit=2, offset=0 → hasMore true（共 5 条）
        let r = svc.fetch_news(FetchNewsRequest {
            limit: Some(2),
            offset: Some(0),
            ..Default::default()
        });
        assert_eq!(r.items.len(), 2);
        assert!(r.page.has_more, "5 total, page of 2 → hasMore");
        // offset=4, limit=2 → 只剩 1 条，hasMore false
        let r = svc.fetch_news(FetchNewsRequest {
            limit: Some(2),
            offset: Some(4),
            ..Default::default()
        });
        assert_eq!(r.items.len(), 1);
        assert!(!r.page.has_more, "last page → no more");
    }

    /// Spec §4: 无 query 时按 publishedAt desc, createdAt desc, id asc 排序；
    /// publishedAt 缺失时用 createdAt 参与第一排序位。
    #[test]
    fn news_spec_fetch_default_ordering_published_desc() {
        let svc = setup_service();
        let t = |s: u32| Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, s).unwrap();
        // 三条：发布时间递增；期望返回倒序（最新在前）。
        insert_item_full(&svc, "id-old", "rss:sample", "old", None, Some(t(10)), t(10));
        insert_item_full(&svc, "id-mid", "rss:sample", "mid", None, Some(t(20)), t(20));
        insert_item_full(&svc, "id-new", "rss:sample", "new", None, Some(t(30)), t(30));
        let r = svc.fetch_news(FetchNewsRequest::default());
        let ids: Vec<&str> = r.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["id-new", "id-mid", "id-old"], "publishedAt DESC");
    }

    /// Spec §4: publishedFrom / publishedTo 是闭区间；publishedAt 缺失的新闻不命中时间过滤。
    #[test]
    fn news_spec_fetch_published_time_range_closed_interval() {
        let svc = setup_service();
        let from = Utc.with_ymd_and_hms(2026, 3, 10, 0, 0, 0).unwrap();
        let to = Utc.with_ymd_and_hms(2026, 3, 20, 0, 0, 0).unwrap();
        // 边界 == from（闭区间命中）
        insert_item_full(&svc, "id-at-from", "rss:sample", "at from", None, Some(from), from);
        // 边界 == to（闭区间命中）
        insert_item_full(&svc, "id-at-to", "rss:sample", "at to", None, Some(to), to);
        // 窗口内
        let mid = Utc.with_ymd_and_hms(2026, 3, 15, 0, 0, 0).unwrap();
        insert_item_full(&svc, "id-mid", "rss:sample", "mid", None, Some(mid), mid);
        // 窗口前
        let before = Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap();
        insert_item_full(&svc, "id-before", "rss:sample", "before", None, Some(before), before);
        // publishedAt 缺失 → 不命中时间过滤
        insert_item_full(&svc, "id-null", "rss:sample", "null pub", None, None, mid);

        let r = svc.fetch_news(FetchNewsRequest {
            published_from: Some(from),
            published_to: Some(to),
            ..Default::default()
        });
        let ids: std::collections::HashSet<&str> =
            r.items.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains("id-at-from"), "closed interval includes from");
        assert!(ids.contains("id-at-to"), "closed interval includes to");
        assert!(ids.contains("id-mid"));
        assert!(!ids.contains("id-before"), "before window excluded");
        assert!(
            !ids.contains("id-null"),
            "publishedAt-null item must not match time range filter"
        );
    }

    /// Spec §4: dateCounts 按北京时区(UTC+8)分组，只计 publishedAt 非空条目，
    /// 与 items 共享 filter 但不分页。
    #[test]
    fn news_spec_fetch_date_counts_beijing_grouping() {
        let svc = setup_service();
        // 2026-03-09 23:30 UTC = 2026-03-10 07:30 北京 → 计入 2026-03-10
        let utc_late = Utc.with_ymd_and_hms(2026, 3, 9, 23, 30, 0).unwrap();
        // 2026-03-10 16:30 UTC = 2026-03-11 00:30 北京 → 计入 2026-03-11
        let utc_cross = Utc.with_ymd_and_hms(2026, 3, 10, 16, 30, 0).unwrap();
        // 另一条同北京日 2026-03-10
        let utc_same = Utc.with_ymd_and_hms(2026, 3, 10, 1, 0, 0).unwrap();
        insert_item_full(&svc, "id-a", "rss:sample", "a", None, Some(utc_late), utc_late);
        insert_item_full(&svc, "id-b", "rss:sample", "b", None, Some(utc_same), utc_same);
        insert_item_full(&svc, "id-c", "rss:sample", "c", None, Some(utc_cross), utc_cross);
        // publishedAt 缺失 → 不计入 dateCounts
        insert_item_full(&svc, "id-null", "rss:sample", "null", None, None, utc_same);

        let r = svc.fetch_news(FetchNewsRequest {
            // 用极小 limit 证明 dateCounts 不受分页限制
            limit: Some(1),
            ..Default::default()
        });
        let counts: HashMap<String, u32> = r
            .date_counts
            .iter()
            .map(|d| (d.date.clone(), d.count))
            .collect();
        assert_eq!(
            counts.get("2026-03-10").copied(),
            Some(2),
            "two items fall on Beijing date 2026-03-10, got {:?}",
            counts
        );
        assert_eq!(counts.get("2026-03-11").copied(), Some(1));
        let total: u32 = counts.values().sum();
        assert_eq!(total, 3, "publishedAt-null item excluded from dateCounts");
    }

    /// Spec §4: fetch_news.sources 含未知 source 返回 invalid_input（不静默当无新闻）。
    #[test]
    fn news_spec_fetch_unknown_source_returns_invalid_input() {
        let svc = setup_service();
        let r = svc.fetch_news(FetchNewsRequest {
            sources: Some(vec!["rss:does-not-exist".to_string()]),
            ..Default::default()
        });
        assert!(r.items.is_empty());
        assert!(
            r.errors.iter().any(|e| e.code == ErrorCode::InvalidInput
                && e.field.as_deref() == Some("sources")),
            "unknown source must yield invalid_input on field=sources, got {:?}",
            r.errors
        );
    }

    /// Spec §4: includeArticle=true 且本地有正文 → 返回 article + articleExcerpt +
    /// freshness.articleFetchedAt；不触发远端抽取（这里直接写本地 ArticleContent）。
    #[test]
    fn news_spec_fetch_include_article_returns_local_content() {
        use crate::domain::news::types::ArticleContent;
        let svc = setup_service();
        let url = "https://example.com/post-x";
        insert_item_full(
            &svc,
            "id-art",
            "rss:sample",
            "headline",
            Some(url),
            Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
            Utc::now(),
        );
        let fetched = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        svc.repo()
            .upsert_article_content(&ArticleContent {
                url: url.to_string(),
                first_news_id: Some("id-art".to_string()),
                title: Some("Article Title".to_string()),
                content: Some(
                    "This is the local article body. It is sufficiently long to be a real article."
                        .to_string(),
                ),
                payload: serde_json::json!({}),
                fetched_at: fetched,
                warning: None,
            })
            .unwrap();

        let r = svc.fetch_news(FetchNewsRequest {
            include_article: Some(true),
            ..Default::default()
        });
        let item = r.items.iter().find(|i| i.id == "id-art").expect("item");
        let art = item.article.as_ref().expect("article present");
        assert!(art.content.contains("local article body"));
        assert_eq!(art.title.as_deref(), Some("Article Title"));
        assert_eq!(art.fetched_at, Some(fetched));
        // articleExcerpt 必须生成
        assert!(item.article_excerpt.is_some(), "excerpt must be present");
        // freshness.articleFetchedAt 必须回填
        assert_eq!(
            item.freshness.as_ref().and_then(|f| f.article_fetched_at),
            Some(fetched)
        );
        // 有正文 → 无 article_missing warning
        assert!(!item.warnings.contains(&WarningCode::ArticleMissing));
    }

    /// Spec §4 / §3: includeArticle=true 但无正文（或失败缓存 content=None）→
    /// 不返回 article 字段，必须返回 article_missing warning。
    #[test]
    fn news_spec_fetch_include_article_missing_yields_warning() {
        use crate::domain::news::types::ArticleContent;
        let svc = setup_service();
        // 1) 完全无正文缓存
        insert_item_full(
            &svc,
            "id-no-cache",
            "rss:sample",
            "no cache",
            Some("https://example.com/no-cache"),
            None,
            Utc::now(),
        );
        // 2) 失败缓存：content=None（只用于抑制重试，不可当可用正文）
        let url2 = "https://example.com/fail-cache";
        insert_item_full(&svc, "id-fail-cache", "rss:sample", "fail cache", Some(url2), None, Utc::now());
        svc.repo()
            .upsert_article_content(&ArticleContent {
                url: url2.to_string(),
                first_news_id: Some("id-fail-cache".to_string()),
                title: None,
                content: None,
                payload: serde_json::json!({"reason": "too_short"}),
                fetched_at: Utc::now(),
                warning: Some(WarningCode::ArticleMissing),
            })
            .unwrap();

        let r = svc.fetch_news(FetchNewsRequest {
            include_article: Some(true),
            ..Default::default()
        });
        for id in ["id-no-cache", "id-fail-cache"] {
            let item = r.items.iter().find(|i| i.id == id).expect("item");
            assert!(item.article.is_none(), "{id}: no article field when content missing");
            assert!(
                item.warnings.contains(&WarningCode::ArticleMissing),
                "{id}: must carry article_missing warning"
            );
        }
    }

    /// Spec §4 (binding 红线): FetchNewsItem.warnings / errors 即使为空也必须序列化为 `[]`，
    /// 不能 skip 成 undefined（前端 .length 崩）。这是近期修复点。
    #[test]
    fn news_spec_fetch_item_warnings_errors_always_serialize_as_array() {
        let svc = setup_service();
        insert_item_full(
            &svc,
            "id-plain",
            "rss:sample",
            "plain item",
            None,
            None,
            Utc::now(),
        );
        let r = svc.fetch_news(FetchNewsRequest::default());
        let item = r.items.first().expect("one item");
        // 该 item 无 warning / error
        assert!(item.warnings.is_empty());
        assert!(item.errors.is_empty());
        // 序列化必须含 "warnings":[] 与 "errors":[]
        let json = serde_json::to_string(item).unwrap();
        assert!(
            json.contains("\"warnings\":[]"),
            "warnings must serialize as [], got: {json}"
        );
        assert!(
            json.contains("\"errors\":[]"),
            "errors must serialize as [], got: {json}"
        );
    }

    /// Spec §4 / §6: list_news_sources 返回编译期 source 集合（bootstrap 后）+ 刷新状态字段。
    #[test]
    fn news_spec_list_news_sources_returns_compile_time_set() {
        let svc = setup_service();
        let resp = svc.list_news_sources();
        assert!(!resp.items.is_empty(), "bootstrap must populate default sources");
        // 每个 source_id 必须是 namespace:channel 形式，且与 NewsItem.source 一致格式。
        for s in &resp.items {
            assert!(
                s.source_id.contains(':'),
                "source_id must be namespace:channel, got {}",
                s.source_id
            );
            assert!(!s.provider.is_empty());
        }
        // 默认集合应含 newsnow:cls-telegraph（registry DEFAULT_SOURCES）。
        assert!(
            resp.items.iter().any(|s| s.source_id == "newsnow:cls-telegraph"),
            "expected default newsnow:cls-telegraph in {:?}",
            resp.items.iter().map(|s| &s.source_id).collect::<Vec<_>>()
        );
    }

    /// Spec §5: warm_articles newsIds 超过 200 → invalid_input，不创建 batchId。
    #[tokio::test]
    async fn news_spec_warm_articles_ids_over_max_rejected() {
        let svc = setup_service();
        let ids: Vec<String> = (0..201).map(|i| format!("id-{i}")).collect();
        let resp = svc
            .warm_articles(WarmArticlesRequest {
                news_ids: Some(ids),
                ..Default::default()
            })
            .await;
        match resp {
            WarmArticlesResponse::Err(e) => {
                assert_eq!(e.error.code, ErrorCode::InvalidInput);
                assert_eq!(e.error.field, Some(WarmArticlesErrorField::NewsIds));
            }
            WarmArticlesResponse::Ok(_) => panic!("expected invalid_input for >200 ids"),
        }
    }

    /// Spec §5: warm_articles 指定的 newsIds 未找到 → ok=false / not_found，不创建 batchId。
    #[tokio::test]
    async fn news_spec_warm_articles_not_found() {
        let svc = setup_service();
        let resp = svc
            .warm_articles(WarmArticlesRequest {
                news_ids: Some(vec!["id-missing-xyz".to_string()]),
                ..Default::default()
            })
            .await;
        match resp {
            WarmArticlesResponse::Err(e) => {
                assert_eq!(e.error.code, ErrorCode::NotFound);
                assert_eq!(e.error.field, Some(WarmArticlesErrorField::NewsIds));
            }
            WarmArticlesResponse::Ok(_) => panic!("expected not_found"),
        }
    }

    /// Spec §2 / §6: 同一条新闻重复刷新（同 ID）不生成多条主记录。
    /// 用 repository 批量 upsert 两次，断言第二次为 Unchanged 且总行数不变。
    #[test]
    fn news_spec_refresh_dedup_no_duplicate_main_record() {
        let svc = setup_service();
        let now = Utc::now();
        let items = vec![NewsItem {
            id: "rss:sample:url:abc".to_string(),
            source: "rss:sample".to_string(),
            title: "dedup me".to_string(),
            summary: None,
            url: Some("https://example.com/dedup".to_string()),
            published_at: None,
            payload: serde_json::json!({}),
            created_at: now,
            updated_at: now,
        }];
        let first = svc.repo().upsert_news_items_batch(&items).unwrap();
        assert_eq!(first[0].1, RepoItemUpsertOutcome::Inserted);
        let second = svc.repo().upsert_news_items_batch(&items).unwrap();
        assert_eq!(
            second[0].1,
            RepoItemUpsertOutcome::Unchanged,
            "same content re-refresh → Unchanged"
        );
        // 主记录只有一条
        let r = svc.fetch_news(FetchNewsRequest::default());
        assert_eq!(r.items.len(), 1, "dedup must keep single main record");
    }

    /// Spec §5 / refresh: run_refresh 对未知 source 返回 invalid_input，不创建 batchId。
    #[tokio::test]
    async fn news_spec_run_refresh_unknown_source_invalid_input() {
        let svc = setup_service();
        let out = svc
            .run_refresh(RefreshBatchInput {
                sources: Some(vec!["rss:nope".to_string()]),
                force: false,
            })
            .await;
        match out {
            RefreshBatchOutcome::Err(e) => assert_eq!(e.code, ErrorCode::InvalidInput),
            RefreshBatchOutcome::Ok(_) => panic!("expected invalid_input for unknown source"),
        }
    }

    /// Spec §5 / rss.md: 单个 source 失败应被隔离（写入 failures），不阻塞其他源。
    /// 隔离原语在 provider adapter 层：feed_url 缺失立即产出 NewsFailure（不发网络），
    /// 且 items 为空。run_refresh 把这些 failure 累积进 payload.failures 而非整批 Err
    /// （registry 编译期常量无法注入 broken source，故在 provider 层验证该原语，
    /// 编排级隔离由 live e2e `news_live_refresh_then_fetch_end_to_end` 覆盖）。
    #[tokio::test]
    async fn news_spec_provider_missing_feed_url_isolated_failure() {
        let broken = NewsSourceRef {
            source_id: "rss:broken".to_string(),
            provider: "rss".to_string(),
            feed_url: None,
            display_name: None,
            enabled: true,
        };
        let rss = RssProvider::new().unwrap();
        let (items, _warns, failure) = rss.fetch(&broken).await;
        assert!(items.is_empty(), "broken source yields no items");
        let f = failure.expect("missing feed_url must produce a failure");
        assert_eq!(f.provider, "rss");
        assert_eq!(f.source.as_deref(), Some("rss:broken"));
        // feed_url 缺失映射 invalid_input（rss adapter, stage=fetch）
        assert_eq!(f.code, ErrorCode::InvalidInput);
        assert_eq!(f.stage, Some(NewsRefreshStage::Fetch));

        // NewsNow 同样：feed_url 缺失 → invalid_input，不发网络。
        let broken_nn = NewsSourceRef {
            source_id: "newsnow:broken".to_string(),
            provider: "newsnow".to_string(),
            feed_url: None,
            display_name: None,
            enabled: true,
        };
        let nn = NewsNowProvider::new().unwrap();
        let (items, _w, failure) = nn.fetch(&broken_nn).await;
        assert!(items.is_empty());
        let f = failure.expect("newsnow missing endpoint must fail");
        assert_eq!(f.code, ErrorCode::InvalidInput);
        assert_eq!(f.provider, "newsnow");
    }

    /// Spec §5: 失败缓存（content=None）首次写入 `news_articles` 时,
    /// `articleUpdatedCount` 与 `articleUpdatedNewsIds` 必须严格 lockstep —
    /// 两者都为空/零，绝不能出现 "ids 非空但 count = 0" 的字段间漂移。
    ///
    /// 构造方法：用一个 URL 但抓取必然失败（example.invalid），warm 路径会写入失败缓存
    /// （content=None），repository 返回 `Updated{affected_news_ids}` 但 service 必须不把
    /// 这些 id 计入 `updated_ids`，也不递增 `article_updated_count`。
    #[tokio::test]
    async fn warm_articles_failure_cache_does_not_leak_into_article_updated_ids() {
        let svc = setup_service();
        let url = "http://news-bc-test-lockstep.invalid/article";
        insert_item(&svc, "id-lockstep", "rss:sample", "lockstep title", Some(url));

        let emitted: Arc<Mutex<Vec<NewsRefreshedPayload>>> = Arc::new(Mutex::new(vec![]));
        let captured = Arc::clone(&emitted);
        svc.set_event_sink(Arc::new(move |p| {
            captured.lock().unwrap().push(p);
        }));

        let resp = svc
            .warm_articles(WarmArticlesRequest {
                news_ids: Some(vec!["id-lockstep".to_string()]),
                force: Some(true),
                ..Default::default()
            })
            .await;
        match resp {
            WarmArticlesResponse::Ok(ok) => {
                // 失败缓存写入成功，但不算 article-update：两字段严格 lockstep。
                assert_eq!(
                    ok.result.article_updated_count, 0,
                    "failure cache must not increment article_updated_count"
                );
                assert!(
                    ok.result.article_updated_news_ids.is_empty(),
                    "failure cache must not populate article_updated_news_ids, got {:?}",
                    ok.result.article_updated_news_ids
                );
                // 同时确认 attempted 已计数且 failure 已产生（确实走到写缓存分支）。
                assert!(!ok.result.failures.is_empty(), "expected article failure");
            }
            WarmArticlesResponse::Err(e) => panic!("unexpected err: {:?}", e.error.code),
        }
        // articleUpdatedCount = 0 → 不 emit news-refreshed（spec §5）。
        assert!(
            emitted.lock().unwrap().is_empty(),
            "no emit when article_updated_count=0"
        );
    }

    // ======================================================================
    // 端到端 live 测试 —— 真实 NewsNow 拉取 → 入本地读模型 → fetch_news 读回。
    // 默认 #[ignore]。运行：
    //   cargo test --manifest-path src-tauri/Cargo.toml \
    //     pipeline::news::service::tests::news_live_ -- --ignored --nocapture
    // 不可达时优雅 skip（打印原因），不 panic。
    // ======================================================================

    /// 真实 run_refresh（默认 NewsNow sources）→ 断言：
    /// - batch Ok；fetched >= saved；新入库 ID 数 = new_ids.len()
    /// - 失败的源进 failures，不阻塞其他源（multi-source aggregation + 隔离）
    /// - fetch_news 能读回这些新闻；每条有 title/source；warnings/errors == []
    #[tokio::test]
    #[ignore]
    async fn news_live_refresh_then_fetch_end_to_end() {
        let svc = setup_service();
        let out = svc.run_refresh(RefreshBatchInput::default()).await;
        let payload = match out {
            RefreshBatchOutcome::Ok(p) => p,
            RefreshBatchOutcome::Err(e) => {
                panic!("refresh should not be invalid_input on default sources: {:?}", e.code)
            }
        };
        println!(
            "[refresh] fetched={} skipped={} saved={} new={} updated={} articleUpdated={} failures={}",
            payload.fetched_count,
            payload.skipped_count,
            payload.saved_count,
            payload.new_ids.len(),
            payload.updated_ids.len(),
            payload.article_updated_count,
            payload.failures.len(),
        );
        for f in &payload.failures {
            println!("    failure: provider={} source={:?} code={:?}", f.provider, f.source, f.code);
        }

        // 至少一个默认源可达并入库；否则全网不可达，按 skip 处理。
        if payload.saved_count == 0 && payload.new_ids.is_empty() {
            eprintln!("[skip] no items saved — all NewsNow sources unreachable?");
            return;
        }
        // savedCount = new + updated（spec §5）
        assert_eq!(
            payload.saved_count as usize,
            payload.new_ids.len() + payload.updated_ids.len()
        );
        // fetched >= saved（去重 + 跳过后保存数不会超过拉取数）
        assert!(payload.fetched_count as usize >= payload.new_ids.len());

        // 读回本地读模型
        let resp = svc.fetch_news(FetchNewsRequest {
            limit: Some(50),
            ..Default::default()
        });
        assert!(!resp.items.is_empty(), "fetch_news should read back saved items");
        assert!(resp.errors.is_empty(), "no top-level errors expected");
        for it in &resp.items {
            assert!(!it.title.trim().is_empty());
            assert!(it.source.starts_with("newsnow:"), "source {} not a newsnow channel", it.source);
            // warnings/errors 恒为 [] (无 includeArticle，无 warning)
            assert!(it.errors.is_empty());
        }
        // dateCounts 应有内容（有 publishedAt 的条目）
        println!("[fetch] {} items read back, {} date buckets", resp.items.len(), resp.date_counts.len());
    }

    /// 真实 warm_articles：先 refresh 拿到带 URL 的新闻，再 warm 抽正文。
    /// 断言：attempted > 0 时，成功抽到的正文非空且合理长度，articleUpdatedCount 与
    /// articleUpdatedNewsIds lockstep；失败的 article-stage failure code = article_extract_failed。
    #[tokio::test]
    #[ignore]
    async fn news_live_warm_articles_extracts_real_body() {
        let svc = setup_service();
        // 只刷 cls-telegraph（有详情页正文，策略 ClsNextData）。
        let out = svc
            .run_refresh(RefreshBatchInput {
                sources: Some(vec!["newsnow:cls-telegraph".to_string()]),
                force: false,
            })
            .await;
        let payload = match out {
            RefreshBatchOutcome::Ok(p) => p,
            RefreshBatchOutcome::Err(e) => panic!("unexpected err {:?}", e.code),
        };
        if payload.new_ids.is_empty() {
            eprintln!("[skip] cls-telegraph unreachable / no new items");
            return;
        }
        // refresh 阶段会同步抓正文；这里再显式 warm 一遍最近条目，强制 force 重抽。
        let resp = svc
            .warm_articles(WarmArticlesRequest {
                recent_limit: Some(10),
                force: Some(true),
                ..Default::default()
            })
            .await;
        let result = match resp {
            WarmArticlesResponse::Ok(ok) => ok.result,
            WarmArticlesResponse::Err(e) => panic!("warm err {:?}", e.error.code),
        };
        println!(
            "[warm] requested={} attempted={} articleUpdated={} failures={}",
            result.requested_count,
            result.attempted_count,
            result.article_updated_count,
            result.failures.len()
        );
        // lockstep：count 与 ids 长度一致
        assert_eq!(
            result.article_updated_count as usize,
            result.article_updated_news_ids.len(),
            "articleUpdatedCount must equal articleUpdatedNewsIds.len()"
        );
        // 任何 article-stage failure 顶层 code 必须是 article_extract_failed + details.reason
        for f in &result.failures {
            if f.stage == Some(NewsRefreshStage::Article) {
                assert_eq!(f.code, ErrorCode::ArticleExtractFailed);
                let reason = f.details.as_ref().and_then(|d| d.get("reason")).and_then(|v| v.as_str());
                assert!(reason.is_some(), "article failure must carry details.reason");
            }
        }
        // 若有正文写入，读回验证非空 + 合理长度
        if result.article_updated_count > 0 {
            let fetched = svc.fetch_news(FetchNewsRequest {
                sources: Some(vec!["newsnow:cls-telegraph".to_string()]),
                include_article: Some(true),
                limit: Some(20),
                ..Default::default()
            });
            let with_article = fetched.items.iter().find(|i| i.article.is_some());
            if let Some(it) = with_article {
                let art = it.article.as_ref().unwrap();
                assert!(!art.content.trim().is_empty(), "extracted body must be non-empty");
                assert!(
                    art.content.chars().count() >= 1,
                    "body length sane: {}",
                    art.content.chars().count()
                );
                // 不含脚本残留
                assert!(!art.content.contains("<script"), "body must not contain raw <script>");
                println!("    body preview: {}", art.content.chars().take(80).collect::<String>());
            }
        } else {
            eprintln!("[note] no article body extracted this run (structures may have changed)");
        }
    }
}
