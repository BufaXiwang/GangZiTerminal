//! News 持久化仓储 — `NewsItem` / `ArticleContent` / `NewsSource` / FTS。
//!
//! Spec: docs/design/news-module.md §2 / §4
//!
//! 责任：
//! - upsert `news_items`（同 ID 冲突时更新 `updated_at` + `payload`；判定是否字段变化）
//! - upsert `news_articles`（按 canonical URL 主键；正文变化同步 FTS）
//! - 维护 `news_search_fts` 与 `news_items` / `news_articles` 的一致性
//! - 根据 `FetchNewsRequest` 拼装查询（按 spec §4 规则）
//! - 不做 provider 调用，不做 normalize；只接受 domain 类型 / canonical URL

use crate::domain::news::source::NewsSource;
use crate::domain::news::types::{ArticleContent, NewsItem, NewsOrder};
use crate::domain::shared::{ErrorCode, OccurredAt, WarningCode};
use crate::infrastructure::db::AppDb;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::Value as JsonValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoItemUpsertOutcome {
    Inserted,
    Updated,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoArticleUpsertOutcome {
    /// 正文写入或更新成功，受影响的 NewsItem.id 列表（共享同 canonical URL）。
    Updated { affected_news_ids: Vec<String> },
    /// 正文与已有缓存等价，未触发 FTS 更新。
    Unchanged,
}

pub struct NewsRepository<'a> {
    db: &'a AppDb,
}

impl<'a> NewsRepository<'a> {
    pub fn new(db: &'a AppDb) -> Self {
        Self { db }
    }

    /// 仓库内部使用：暴露底层 `AppDb` 给同 crate 模块（registry 等）。
    pub(crate) fn db_for_test_only(&self) -> &AppDb {
        self.db
    }

    // -- NewsItem upsert -----------------------------------------------------

    /// Upsert 一条 NewsItem。返回 outcome 用于 refresh 统计。
    ///
    /// Spec: news-module.md §2 / §4
    /// - 同 ID 冲突时更新 `updated_at` 和 `payload`，不新建记录。
    /// - title / summary / url / publishedAt / payload 任一字段变化算 "updated"。
    pub fn upsert_news_item(&self, item: &NewsItem) -> rusqlite::Result<RepoItemUpsertOutcome> {
        self.db.with(|conn| {
            let tx = conn.transaction()?;
            let outcome = upsert_news_item_in_tx(&tx, item)?;
            tx.commit()?;
            Ok(outcome)
        })
    }

    /// 批量 upsert（refresh 路径用）。在单个 transaction 中处理。
    pub fn upsert_news_items_batch(
        &self,
        items: &[NewsItem],
    ) -> rusqlite::Result<Vec<(String, RepoItemUpsertOutcome)>> {
        self.db.with(|conn| {
            let tx = conn.transaction()?;
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let outcome = upsert_news_item_in_tx(&tx, item)?;
                out.push((item.id.clone(), outcome));
            }
            tx.commit()?;
            Ok(out)
        })
    }

    // -- ArticleContent ------------------------------------------------------

    /// Upsert 一条 ArticleContent；正文变化时回写 FTS 的 `article` 列。
    ///
    /// Spec: news-module.md §2
    /// - 多条 NewsItem 共享同 canonical URL → 同步更新 FTS。
    /// - `content` 缺失（失败缓存）也写入，但 FTS 不写入正文。
    pub fn upsert_article_content(
        &self,
        article: &ArticleContent,
    ) -> rusqlite::Result<RepoArticleUpsertOutcome> {
        self.db.with(|conn| {
            let tx = conn.transaction()?;
            let outcome = upsert_article_in_tx(&tx, article)?;
            tx.commit()?;
            Ok(outcome)
        })
    }

    /// 仅查询 ArticleContent；用于读取路径。
    pub fn get_article_content(&self, canonical_url: &str) -> rusqlite::Result<Option<ArticleContent>> {
        self.db.with(|conn| {
            conn.query_row(
                "SELECT url, first_news_id, title, content, payload_json, fetched_at, warning
                 FROM news_articles WHERE url = ?1",
                [canonical_url],
                row_to_article,
            )
            .optional()
        })
    }

    /// 取一条 NewsItem。
    pub fn get_news_item(&self, id: &str) -> rusqlite::Result<Option<NewsItem>> {
        self.db.with(|conn| {
            conn.query_row(
                "SELECT id, source, title, summary, url, published_at, payload_json, created_at, updated_at
                 FROM news_items WHERE id = ?1",
                [id],
                row_to_news_item,
            )
            .optional()
        })
    }

    /// 批量取 NewsItem，保持输入顺序（spec §4 `ids` 查询）。
    pub fn get_news_items_by_ids(
        &self,
        ids: &[String],
    ) -> rusqlite::Result<Vec<Option<NewsItem>>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        self.db.with(|conn| {
            // 简单实现：逐条查（数量上限 200 个，spec §查询规模限制）
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                let item = conn
                    .query_row(
                        "SELECT id, source, title, summary, url, published_at, payload_json, created_at, updated_at
                         FROM news_items WHERE id = ?1",
                        [id],
                        row_to_news_item,
                    )
                    .optional()?;
                out.push(item);
            }
            Ok(out)
        })
    }

    /// 查询 NewsItem 列表（按 spec §4 过滤 + 排序）。
    ///
    /// 返回 (items, total_matching) — total 用于 `hasMore` 计算。
    #[allow(clippy::too_many_arguments)]
    pub fn list_news_items(
        &self,
        sources: Option<&[String]>,
        published_from: Option<&DateTime<Utc>>,
        published_to: Option<&DateTime<Utc>>,
        query: Option<&str>,
        order: NewsOrder,
        limit: u32,
        offset: u32,
    ) -> rusqlite::Result<ListResult> {
        self.db.with(|conn| {
            list_news_items_impl(
                conn,
                sources,
                published_from,
                published_to,
                query,
                order,
                limit,
                offset,
            )
        })
    }

    /// 按北京日期统计每日条数（同 filter，不分页），供日期导航显示真实总数。
    pub fn count_news_by_date(
        &self,
        sources: Option<&[String]>,
        published_from: Option<&DateTime<Utc>>,
        published_to: Option<&DateTime<Utc>>,
        query: Option<&str>,
    ) -> rusqlite::Result<Vec<(String, u32)>> {
        self.db
            .with(|conn| count_news_by_date_impl(conn, sources, published_from, published_to, query))
    }

    /// 按 newsId 列表精确取回（Runtime 装载 news buffer 批次用）。
    ///
    /// Spec: news-module.md §4 `fetch_news` 的 `ids` 路径。上限 200；按时间倒序；未命中按缺失忽略。
    pub fn list_news_by_ids(
        &self,
        ids: &[String],
        limit: u32,
        offset: u32,
    ) -> rusqlite::Result<ListResult> {
        if ids.is_empty() {
            return Ok(ListResult {
                items: vec![],
                total: 0,
            });
        }
        let capped: Vec<String> = ids.iter().take(200).cloned().collect();
        self.db.with(|conn| {
            let marks: Vec<String> = (0..capped.len()).map(|_| "?".to_string()).collect();
            let from_with_where = format!("FROM news_items ni WHERE ni.id IN ({})", marks.join(","));
            let count_sql = format!("SELECT COUNT(*) {}", from_with_where);
            let select_sql = format!(
                "SELECT ni.id, ni.source, ni.title, ni.summary, ni.url, ni.published_at,
                        ni.payload_json, ni.created_at, ni.updated_at
                 {} ORDER BY COALESCE(ni.published_at, ni.created_at) DESC, ni.created_at DESC, ni.id ASC
                 LIMIT ? OFFSET ?",
                from_with_where
            );
            let mut binds: Vec<rusqlite::types::Value> = capped
                .iter()
                .map(|s| rusqlite::types::Value::Text(s.clone()))
                .collect();
            let total: i64 = {
                let mut stmt = conn.prepare(&count_sql)?;
                stmt.query_row(rusqlite::params_from_iter(binds.iter()), |r| r.get(0))?
            };
            binds.push(rusqlite::types::Value::Integer(limit as i64));
            binds.push(rusqlite::types::Value::Integer(offset as i64));
            let mut stmt = conn.prepare(&select_sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(binds.iter()), row_to_news_item)?;
            let mut items = Vec::new();
            for r in rows {
                items.push(r?);
            }
            Ok(ListResult {
                items,
                total: total.max(0) as u32,
            })
        })
    }

    // -- NewsSource ----------------------------------------------------------

    pub fn list_sources(&self) -> rusqlite::Result<Vec<NewsSource>> {
        self.db.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT source_id, provider, display_name, enabled, dynamic,
                        last_refresh_at, last_error_code, last_error_message, last_error_at
                 FROM news_sources ORDER BY source_id",
            )?;
            let rows = stmt.query_map([], row_to_source)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    pub fn get_source(&self, source_id: &str) -> rusqlite::Result<Option<NewsSource>> {
        self.db.with(|conn| {
            conn.query_row(
                "SELECT source_id, provider, display_name, enabled, dynamic,
                        last_refresh_at, last_error_code, last_error_message, last_error_at
                 FROM news_sources WHERE source_id = ?1",
                [source_id],
                row_to_source,
            )
            .optional()
        })
    }

    /// Upsert source（用于启动时同步配置）。
    pub fn upsert_source(
        &self,
        source_id: &str,
        provider: &str,
        display_name: Option<&str>,
        enabled: bool,
        feed_url: Option<&str>,
        now: DateTime<Utc>,
    ) -> rusqlite::Result<()> {
        let now_str = format_dt(&now);
        self.db.with(|conn| {
            conn.execute(
                "INSERT INTO news_sources (source_id, provider, display_name, enabled, feed_url, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                 ON CONFLICT(source_id) DO UPDATE SET
                    provider = excluded.provider,
                    display_name = excluded.display_name,
                    enabled = excluded.enabled,
                    feed_url = excluded.feed_url,
                    updated_at = excluded.updated_at",
                params![
                    source_id,
                    provider,
                    display_name,
                    enabled as i64,
                    feed_url,
                    now_str,
                ],
            )?;
            Ok(())
        })
    }

    /// 删除一个 source（清理旧 source_id 用；当前只在 registry bootstrap 调用）。
    pub fn delete_source(&self, source_id: &str) -> rusqlite::Result<()> {
        self.db.with(|conn| {
            conn.execute("DELETE FROM news_sources WHERE source_id = ?1", params![source_id])?;
            Ok(())
        })
    }

    /// 删除一条 news_item 及其 FTS 索引行（保持 FTS 与 news_items 一致）。
    /// news_articles 按 url 存、可被同 url 的其他 item 复用，故不在此删除。
    pub fn delete_news_item(&self, news_id: &str) -> rusqlite::Result<()> {
        self.db.with(|conn| {
            conn.execute("DELETE FROM news_search_fts WHERE news_id = ?1", params![news_id])?;
            conn.execute("DELETE FROM news_items WHERE id = ?1", params![news_id])?;
            Ok(())
        })
    }

    /// 启动自愈：清掉 FTS 中指向已不存在 news_item 的幽灵行。
    /// `news_search_fts` 是独立 FTS5 表（article 列来自 news_articles join，无法做
    /// external-content + 触发器），靠本对账保证不随历史删除累积幽灵行。返回清理条数。
    /// Spec: news-module.md §2 全文搜索（FTS 与 news_items 一致性）。
    pub fn prune_fts_orphans(&self) -> rusqlite::Result<usize> {
        self.db.with(|conn| {
            let n = conn.execute(
                "DELETE FROM news_search_fts
                 WHERE news_id NOT IN (SELECT id FROM news_items)",
                [],
            )?;
            Ok(n)
        })
    }

    pub fn record_source_refresh_ok(&self, source_id: &str, when: DateTime<Utc>) -> rusqlite::Result<()> {
        let when_s = format_dt(&when);
        self.db.with(|conn| {
            conn.execute(
                "UPDATE news_sources SET last_refresh_at = ?2, last_error_code = NULL,
                    last_error_message = NULL, last_error_at = NULL, updated_at = ?2
                 WHERE source_id = ?1",
                params![source_id, when_s],
            )?;
            Ok(())
        })
    }

    pub fn record_source_refresh_err(
        &self,
        source_id: &str,
        code: ErrorCode,
        message: Option<&str>,
        when: DateTime<Utc>,
    ) -> rusqlite::Result<()> {
        let when_s = format_dt(&when);
        let code_s = serde_plain_code(code);
        self.db.with(|conn| {
            conn.execute(
                "UPDATE news_sources SET last_error_code = ?2, last_error_message = ?3,
                    last_error_at = ?4, updated_at = ?4
                 WHERE source_id = ?1",
                params![source_id, code_s, message, when_s],
            )?;
            Ok(())
        })
    }

    /// 选最近 N 条有 URL 且未有成功正文缓存的新闻（warm_articles 用）。
    pub fn select_recent_for_warm(&self, recent_limit: u32) -> rusqlite::Result<Vec<NewsItem>> {
        self.db.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, source, title, summary, url, published_at, payload_json, created_at, updated_at
                 FROM news_items
                 WHERE url IS NOT NULL
                 ORDER BY COALESCE(published_at, created_at) DESC, id
                 LIMIT ?1",
            )?;
            let rows = stmt.query_map([recent_limit as i64], row_to_news_item)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 找所有 url 等于 canonical_url 的 NewsItem.id（spec §2 多条共享同一 URL）。
    pub fn find_news_ids_by_url(&self, canonical_url: &str) -> rusqlite::Result<Vec<String>> {
        self.db.with(|conn| {
            let mut stmt = conn.prepare("SELECT id FROM news_items WHERE url = ?1")?;
            let rows = stmt.query_map([canonical_url], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }
}

// -- 内部 helper -------------------------------------------------------------

fn upsert_news_item_in_tx(
    tx: &Transaction<'_>,
    item: &NewsItem,
) -> rusqlite::Result<RepoItemUpsertOutcome> {
    // 取既有记录用于判定 inserted/updated/unchanged
    let existing = tx
        .query_row(
            "SELECT title, summary, url, published_at, payload_json
             FROM news_items WHERE id = ?1",
            [&item.id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;

    let payload_str = serde_json::to_string(&item.payload).unwrap_or_else(|_| "{}".to_string());
    let published_at_s = item.published_at.as_ref().map(format_dt);
    let created_at_s = format_dt(&item.created_at);
    let updated_at_s = format_dt(&item.updated_at);

    match existing {
        None => {
            tx.execute(
                "INSERT INTO news_items
                   (id, source, title, summary, url, published_at, payload_json, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    &item.id,
                    &item.source,
                    &item.title,
                    &item.summary,
                    &item.url,
                    &published_at_s,
                    &payload_str,
                    &created_at_s,
                    &updated_at_s,
                ],
            )?;
            sync_fts_item(tx, &item.id, &item.title, item.summary.as_deref())?;
            Ok(RepoItemUpsertOutcome::Inserted)
        }
        Some((e_title, e_summary, e_url, e_pub, e_payload)) => {
            let changed = e_title != item.title
                || e_summary != item.summary
                || e_url != item.url
                || e_pub != published_at_s
                || e_payload != payload_str;
            if changed {
                tx.execute(
                    "UPDATE news_items SET title = ?2, summary = ?3, url = ?4,
                        published_at = ?5, payload_json = ?6, updated_at = ?7
                     WHERE id = ?1",
                    params![
                        &item.id,
                        &item.title,
                        &item.summary,
                        &item.url,
                        &published_at_s,
                        &payload_str,
                        &updated_at_s,
                    ],
                )?;
                sync_fts_item(tx, &item.id, &item.title, item.summary.as_deref())?;
                Ok(RepoItemUpsertOutcome::Updated)
            } else {
                Ok(RepoItemUpsertOutcome::Unchanged)
            }
        }
    }
}

fn upsert_article_in_tx(
    tx: &Transaction<'_>,
    article: &ArticleContent,
) -> rusqlite::Result<RepoArticleUpsertOutcome> {
    let existing_content: Option<Option<String>> = tx
        .query_row(
            "SELECT content FROM news_articles WHERE url = ?1",
            [&article.url],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?;

    let payload_str = serde_json::to_string(&article.payload).unwrap_or_else(|_| "{}".to_string());
    let fetched_at_s = format_dt(&article.fetched_at);
    let warning_s = article.warning.map(serde_plain_warning);

    tx.execute(
        "INSERT INTO news_articles (url, first_news_id, title, content, payload_json, fetched_at, warning)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(url) DO UPDATE SET
            first_news_id = COALESCE(news_articles.first_news_id, excluded.first_news_id),
            title = excluded.title,
            content = excluded.content,
            payload_json = excluded.payload_json,
            fetched_at = excluded.fetched_at,
            warning = excluded.warning",
        params![
            &article.url,
            &article.first_news_id,
            &article.title,
            &article.content,
            &payload_str,
            &fetched_at_s,
            &warning_s,
        ],
    )?;

    // 内容真正变化才同步 FTS（避免无谓的写入）
    let new_content_opt = article.content.clone();
    let same = matches!(&existing_content, Some(prev) if prev == &new_content_opt);
    if same {
        return Ok(RepoArticleUpsertOutcome::Unchanged);
    }

    // 找出所有 url 等于本 URL 的 NewsItem.id，同步 FTS `article` 列
    let mut stmt = tx.prepare("SELECT id FROM news_items WHERE url = ?1")?;
    let mut affected = Vec::new();
    let rows = stmt.query_map([&article.url], |r| r.get::<_, String>(0))?;
    for r in rows {
        affected.push(r?);
    }
    drop(stmt);

    // 失败缓存（content = None）时，把 FTS 的 article 列清空
    let fts_article = article.content.as_deref().unwrap_or("");
    for id in &affected {
        tx.execute(
            "UPDATE news_search_fts SET article = ?2 WHERE news_id = ?1",
            params![id, fts_article],
        )?;
    }
    Ok(RepoArticleUpsertOutcome::Updated {
        affected_news_ids: affected,
    })
}

fn sync_fts_item(
    tx: &Transaction<'_>,
    news_id: &str,
    title: &str,
    summary: Option<&str>,
) -> rusqlite::Result<()> {
    // 如果存在就更新 title / summary；不存在则插入（article 默认空）
    let existed: Option<i64> = tx
        .query_row(
            "SELECT rowid FROM news_search_fts WHERE news_id = ?1",
            [news_id],
            |r| r.get(0),
        )
        .optional()?;
    if existed.is_some() {
        tx.execute(
            "UPDATE news_search_fts SET title = ?2, summary = ?3 WHERE news_id = ?1",
            params![news_id, title, summary.unwrap_or("")],
        )?;
    } else {
        // article 列：如果该 url 已有 news_articles.content，预填；否则空。
        let article_text: String = tx
            .query_row(
                "SELECT COALESCE(ac.content, '')
                 FROM news_items ni LEFT JOIN news_articles ac ON ac.url = ni.url
                 WHERE ni.id = ?1",
                [news_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_default();
        tx.execute(
            "INSERT INTO news_search_fts (news_id, title, summary, article) VALUES (?1, ?2, ?3, ?4)",
            params![news_id, title, summary.unwrap_or(""), article_text],
        )?;
    }
    Ok(())
}

#[derive(Debug)]
pub struct ListResult {
    pub items: Vec<NewsItem>,
    pub total: u32,
}

struct NewsFilter {
    from_table_sql: String,
    where_sql: String,
    binds: Vec<rusqlite::types::Value>,
    has_query: bool,
}

/// 构建 news 查询的 FROM + WHERE + binds（严格 lockstep，保证 placeholder 顺序）。
/// Spec: news-module.md §4 fetch_news（query / sources / 时间范围按 AND 组合）。
///
/// 历史 bug（B1）：FTS `MATCH ?` 必须在 WHERE 第一位且 bind 第一个；sources/from/to
/// 顺序无关但 clause 与 bind 必须同步 push。返回 None 表示 empty-sources（不命中）。
fn build_news_filter(
    sources: Option<&[String]>,
    published_from: Option<&DateTime<Utc>>,
    published_to: Option<&DateTime<Utc>>,
    query: Option<&str>,
) -> Option<NewsFilter> {
    type ClauseBinds = (String, Vec<rusqlite::types::Value>);
    let mut where_parts: Vec<ClauseBinds> = Vec::new();

    let has_query = query
        .map(|q| !normalize_query_text(q).is_empty())
        .unwrap_or(false);

    let from_table_sql = if has_query {
        let q_norm = normalize_query_text(query.unwrap());
        where_parts.push((
            "fts.news_search_fts MATCH ?".to_string(),
            vec![rusqlite::types::Value::Text(q_norm)],
        ));
        "FROM news_items ni
         INNER JOIN news_search_fts fts ON fts.news_id = ni.id"
            .to_string()
    } else {
        "FROM news_items ni".to_string()
    };

    if let Some(srcs) = sources {
        if !srcs.is_empty() {
            let marks: Vec<String> = (0..srcs.len()).map(|_| "?".to_string()).collect();
            let binds: Vec<rusqlite::types::Value> = srcs
                .iter()
                .map(|s| rusqlite::types::Value::Text(s.clone()))
                .collect();
            where_parts.push((format!("ni.source IN ({})", marks.join(",")), binds));
        } else {
            return None;
        }
    }
    if let Some(from) = published_from {
        where_parts.push((
            "(ni.published_at IS NOT NULL AND ni.published_at >= ?)".to_string(),
            vec![rusqlite::types::Value::Text(format_dt(from))],
        ));
    }
    if let Some(to) = published_to {
        where_parts.push((
            "(ni.published_at IS NOT NULL AND ni.published_at <= ?)".to_string(),
            vec![rusqlite::types::Value::Text(format_dt(to))],
        ));
    }

    let mut where_sql = String::new();
    let mut binds: Vec<rusqlite::types::Value> = Vec::new();
    for (i, (clause, part_binds)) in where_parts.iter().enumerate() {
        where_sql.push_str(if i == 0 { " WHERE " } else { " AND " });
        where_sql.push_str(clause);
        binds.extend(part_binds.iter().cloned());
    }
    if where_sql.is_empty() {
        where_sql = " WHERE 1=1".to_string();
    }
    Some(NewsFilter {
        from_table_sql,
        where_sql,
        binds,
        has_query,
    })
}

/// 按北京日期（UTC+8）分组统计条数，用于资讯页日期导航的每日真实总数。
/// 同 filter（sources / query / 时间范围），但不分页。
/// 返回 `(YYYY-MM-DD, count)`，只统计 `published_at` 非空的条目。
fn count_news_by_date_impl(
    conn: &Connection,
    sources: Option<&[String]>,
    published_from: Option<&DateTime<Utc>>,
    published_to: Option<&DateTime<Utc>>,
    query: Option<&str>,
) -> rusqlite::Result<Vec<(String, u32)>> {
    let Some(filter) = build_news_filter(sources, published_from, published_to, query) else {
        return Ok(vec![]);
    };
    // where_sql 永不为空（至少 "WHERE 1=1"），可安全追加 AND。
    let sql = format!(
        "SELECT date(ni.published_at, '+8 hours') AS d, COUNT(*) AS c
         {}{} AND ni.published_at IS NOT NULL
         GROUP BY d ORDER BY d DESC",
        filter.from_table_sql, filter.where_sql
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(filter.binds.iter()), |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u32))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn list_news_items_impl(
    conn: &Connection,
    sources: Option<&[String]>,
    published_from: Option<&DateTime<Utc>>,
    published_to: Option<&DateTime<Utc>>,
    query: Option<&str>,
    order: NewsOrder,
    limit: u32,
    offset: u32,
) -> rusqlite::Result<ListResult> {
    // filter 构建（FROM/WHERE/binds）抽到 build_news_filter 共享给 count-by-date。
    let Some(filter) = build_news_filter(sources, published_from, published_to, query) else {
        // empty sources list 过滤等价于不命中
        return Ok(ListResult {
            items: vec![],
            total: 0,
        });
    };
    // Spec: news-module.md §4 `order` 语义。
    // - Desc（默认）：有 query 走 FTS relevance（rank）+ `published_at desc, created_at desc, id asc`
    //   tiebreak；无 query 走 `COALESCE(published_at,created_at) desc, created_at desc, id asc`。
    // - Asc：整体时间升序 `COALESCE(published_at,created_at) asc, created_at asc, id asc`。
    //   **有 query 时 asc 也走时间升序为主序（不走 rank）**——keyset 翻页需要时间单调。
    let order_sql = match order {
        NewsOrder::Desc => {
            if filter.has_query {
                "ORDER BY rank, ni.published_at DESC, ni.created_at DESC, ni.id ASC".to_string()
            } else {
                "ORDER BY COALESCE(ni.published_at, ni.created_at) DESC, ni.created_at DESC, ni.id ASC"
                    .to_string()
            }
        }
        NewsOrder::Asc => {
            "ORDER BY COALESCE(ni.published_at, ni.created_at) ASC, ni.created_at ASC, ni.id ASC"
                .to_string()
        }
    };
    let binds = filter.binds;

    let from_with_where = format!("{}{}", filter.from_table_sql, filter.where_sql);
    let count_sql = format!("SELECT COUNT(*) {}", from_with_where);
    let select_sql = format!(
        "SELECT ni.id, ni.source, ni.title, ni.summary, ni.url, ni.published_at,
                ni.payload_json, ni.created_at, ni.updated_at
         {} {} LIMIT ? OFFSET ?",
        from_with_where, order_sql
    );

    let total: u32 = {
        let mut stmt = conn.prepare(&count_sql)?;
        let total: i64 = stmt.query_row(rusqlite::params_from_iter(binds.iter()), |r| r.get(0))?;
        total.max(0) as u32
    };

    let items: Vec<NewsItem> = {
        let mut bind_with_page = binds.clone();
        bind_with_page.push(rusqlite::types::Value::Integer(limit as i64));
        bind_with_page.push(rusqlite::types::Value::Integer(offset as i64));
        let mut stmt = conn.prepare(&select_sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(bind_with_page.iter()),
            row_to_news_item,
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        out
    };

    Ok(ListResult { items, total })
}

/// Normalize 用户传入的 `query`，输出 FTS5 MATCH 表达式。
///
/// Spec: news-module.md §4 lines 265-266
/// - trim、折叠连续空白
/// - **英文大小写不敏感**：FTS5 unicode61 tokenizer 默认大小写折叠，这里再保险一次
///   把 ASCII 字母转小写，避免任何上游 tokenizer 配置漂移导致大小写敏感泄漏。
///   中文按 tokenizer 规则处理（unicode61 已折叠 NFC + casefold）。
/// - **FTS5 特殊字符 strip**：避免调用方传入 `"`、`+`、`-`、`*`、`(`、`)`、`:` 等被当成
///   FTS5 运算符执行（spec §4 line 266：多词 query 的 AND / OR / phrase 行为由 News FTS
///   读模型统一定义，不能由调用方各自解释）。`-` 在 FTS5 中是 NOT/列限定符，`_` 是 token
///   分隔符之一，统一作为 token 分隔符处理 —— 只保留 unicode 字母数字。
/// - 多个非空 token 之间以空格分隔（FTS5 默认 AND 语义）。
/// - 返回空串表示"无可执行 query"，调用方按"无 query"路径处理。
pub(crate) fn normalize_query_text(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    let mut last_ws_or_empty = true;
    for ch in q.chars() {
        if ch.is_alphanumeric() {
            if ch.is_ascii_uppercase() {
                out.push(ch.to_ascii_lowercase());
            } else {
                out.push(ch);
            }
            last_ws_or_empty = false;
        } else {
            // 空白、FTS5 特殊字符、其他标点统一作为 token 分隔
            if !last_ws_or_empty {
                out.push(' ');
                last_ws_or_empty = true;
            }
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

fn row_to_news_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<NewsItem> {
    let payload_json: String = r.get(6)?;
    let payload: JsonValue = serde_json::from_str(&payload_json).unwrap_or(JsonValue::Null);
    Ok(NewsItem {
        id: r.get(0)?,
        source: r.get(1)?,
        title: r.get(2)?,
        summary: r.get(3)?,
        url: r.get(4)?,
        published_at: opt_dt(r, 5)?,
        payload,
        created_at: req_dt(r, 7)?,
        updated_at: req_dt(r, 8)?,
    })
}

fn row_to_article(r: &rusqlite::Row<'_>) -> rusqlite::Result<ArticleContent> {
    let payload_json: String = r.get(4)?;
    let payload: JsonValue = serde_json::from_str(&payload_json).unwrap_or(JsonValue::Null);
    let warning_str: Option<String> = r.get(6)?;
    let warning = warning_str.and_then(parse_warning);
    Ok(ArticleContent {
        url: r.get(0)?,
        first_news_id: r.get(1)?,
        title: r.get(2)?,
        content: r.get(3)?,
        payload,
        fetched_at: req_dt(r, 5)?,
        warning,
    })
}

fn row_to_source(r: &rusqlite::Row<'_>) -> rusqlite::Result<NewsSource> {
    let enabled: i64 = r.get(3)?;
    let dynamic: Option<i64> = r.get(4)?;
    let last_refresh_at: Option<DateTime<Utc>> = opt_dt(r, 5)?;
    let last_error_code: Option<String> = r.get(6)?;
    let last_error_message: Option<String> = r.get(7)?;
    let last_error_at: Option<DateTime<Utc>> = opt_dt(r, 8)?;
    let last_error = match (last_error_code, last_error_at) {
        (Some(code_s), Some(occurred_at)) => parse_error_code(&code_s).map(|code| {
            crate::domain::news::source::NewsSourceLastError {
                code,
                message: last_error_message,
                occurred_at,
            }
        }),
        _ => None,
    };
    Ok(NewsSource {
        source_id: r.get(0)?,
        provider: r.get(1)?,
        display_name: r.get(2)?,
        enabled: enabled != 0,
        dynamic: dynamic.map(|d| d != 0),
        last_refresh_at,
        last_error,
    })
}

fn opt_dt(r: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Option<OccurredAt>> {
    let s: Option<String> = r.get(idx)?;
    Ok(s.and_then(|x| parse_dt(&x)))
}

fn req_dt(r: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<OccurredAt> {
    let s: String = r.get(idx)?;
    parse_dt(&s).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("bad datetime: {}", s),
            )),
        )
    })
}

fn parse_dt(s: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

pub(crate) fn format_dt(d: &DateTime<Utc>) -> String {
    d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn serde_plain_code(code: ErrorCode) -> String {
    // shared/codes.rs 标记 snake_case；serde_json 序列化字符串后去掉外层引号
    serde_json::to_string(&code)
        .ok()
        .and_then(|s| s.strip_prefix('"').and_then(|x| x.strip_suffix('"')).map(|s| s.to_string()))
        .unwrap_or_else(|| "db_error".to_string())
}

fn serde_plain_warning(code: WarningCode) -> String {
    serde_json::to_string(&code)
        .ok()
        .and_then(|s| s.strip_prefix('"').and_then(|x| x.strip_suffix('"')).map(|s| s.to_string()))
        .unwrap_or_default()
}

fn parse_warning(s: String) -> Option<WarningCode> {
    serde_json::from_str(&format!("\"{}\"", s)).ok()
}

fn parse_error_code(s: &str) -> Option<ErrorCode> {
    serde_json::from_str(&format!("\"{}\"", s)).ok()
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::db::{run_migrations, AppDb};
    use crate::infrastructure::news::migrations::migrations;
    use chrono::TimeZone;

    fn setup() -> AppDb {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, migrations()).unwrap());
        db
    }

    fn sample_item(id: &str, source: &str, url: Option<&str>) -> NewsItem {
        NewsItem {
            id: id.to_string(),
            source: source.to_string(),
            title: "hello".to_string(),
            summary: Some("brief".to_string()),
            url: url.map(|s| s.to_string()),
            published_at: Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
            payload: serde_json::json!({"raw": 1}),
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        }
    }

    #[test]
    fn upsert_inserts_then_unchanged() {
        let db = setup();
        let repo = NewsRepository::new(&db);
        let it = sample_item("rss:x:url:abc", "rss:x", Some("https://a.com/x"));
        assert_eq!(repo.upsert_news_item(&it).unwrap(), RepoItemUpsertOutcome::Inserted);
        assert_eq!(repo.upsert_news_item(&it).unwrap(), RepoItemUpsertOutcome::Unchanged);
        let mut it2 = it.clone();
        it2.title = "hello v2".to_string();
        assert_eq!(repo.upsert_news_item(&it2).unwrap(), RepoItemUpsertOutcome::Updated);
    }

    #[test]
    fn fts_search_returns_matching_items() {
        let db = setup();
        let repo = NewsRepository::new(&db);
        let mut a = sample_item("id-a", "rss:x", None);
        a.title = "GangZi quant".to_string();
        let mut b = sample_item("id-b", "rss:x", None);
        b.title = "completely different topic".to_string();
        repo.upsert_news_item(&a).unwrap();
        repo.upsert_news_item(&b).unwrap();

        let r = repo
            .list_news_items(None, None, None, Some("gangzi"), NewsOrder::Desc, 50, 0)
            .unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].id, "id-a");
    }

    // 中文关键词 FTS 行为（tokenize=trigram）：≥3 字子串命中（中英文皆可），2 字受 trigram 固有限制。
    #[test]
    fn fts_chinese_keyword_trigram() {
        let db = setup();
        let repo = NewsRepository::new(&db);
        let mk = |id: &str, title: &str| {
            let mut it = sample_item(id, "rss:x", None);
            it.title = title.to_string();
            it
        };
        // A: 关键词嵌在无标点的长汉字串里（unicode61 下搜不到，trigram 能子串命中）
        repo.upsert_news_item(&mk("emb", "贵州茅台发布2024年业绩预增公告大幅增长")).unwrap();
        // B: 关键词被标点分隔
        repo.upsert_news_item(&mk("punc", "贵州茅台：2024年业绩预增，净利润大增")).unwrap();
        // C: 英文对照
        repo.upsert_news_item(&mk("en", "Kweichow Moutai profit surge")).unwrap();

        let q = |kw: &str| {
            let mut ids = repo
                .list_news_items(None, None, None, Some(kw), NewsOrder::Desc, 50, 0)
                .unwrap()
                .items
                .into_iter()
                .map(|i| i.id)
                .collect::<Vec<_>>();
            ids.sort();
            ids
        };
        println!("\n=== 中文 FTS 诊断（tokenize=trigram）===");
        for kw in ["贵州茅台", "茅台", "业绩", "业绩预增", "moutai", "profit"] {
            println!("  query {:>10}  → {:?}", kw, q(kw));
        }
        println!("===========================================\n");

        // ≥3 字关键词：中英文都做真子串检索，嵌入式也命中（trigram 相比 unicode61 的关键提升）。
        assert_eq!(q("贵州茅台"), vec!["emb", "punc"], "≥3 字中文子串：嵌入式 + 标点分隔都命中");
        assert_eq!(q("业绩预增"), vec!["emb", "punc"], "≥3 字中文子串命中");
        assert_eq!(q("moutai"), vec!["en"], "英文大小写不敏感命中");
        assert_eq!(q("profit"), vec!["en"], "英文命中");
        // trigram 固有限制：query < 3 字符无法用 trigram 索引 → 2 字中文词不命中。
        // （要支持 2 字需 CJK bigram 预切分或 ICU tokenizer；当前权衡为不引入。）
        assert!(q("茅台").is_empty(), "2 字中文受 trigram ≥3 限制，已知不命中");
        assert!(q("业绩").is_empty(), "2 字中文受 trigram ≥3 限制，已知不命中");
    }

    #[test]
    fn delete_news_item_and_prune_fts_keep_search_consistent() {
        let db = setup();
        let repo = NewsRepository::new(&db);
        let mut a = sample_item("id-a", "rss:x", None);
        a.title = "GangZi quant alpha".to_string();
        let mut b = sample_item("id-b", "rss:x", None);
        b.title = "GangZi quant beta".to_string();
        repo.upsert_news_item(&a).unwrap();
        repo.upsert_news_item(&b).unwrap();

        // delete_news_item 同步清 FTS：搜索不再命中已删条目。
        repo.delete_news_item("id-a").unwrap();
        let r = repo.list_news_items(None, None, None, Some("alpha"), NewsOrder::Desc, 50, 0).unwrap();
        assert_eq!(r.items.len(), 0, "deleted item must not be searchable");
        let r2 = repo.list_news_items(None, None, None, Some("beta"), NewsOrder::Desc, 50, 0).unwrap();
        assert_eq!(r2.items.len(), 1);

        // 制造幽灵：绕过 delete_news_item 直接删 news_items 行，留下孤儿 FTS 行。
        db.with(|c| {
            c.execute("DELETE FROM news_items WHERE id = ?1", params!["id-b"]).unwrap();
        });
        // prune 前：FTS 仍有 id-b 的孤儿行。
        let ghosts: i64 = db.with(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM news_search_fts WHERE news_id NOT IN (SELECT id FROM news_items)",
                [],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(ghosts, 1);
        // prune 清掉孤儿。
        assert_eq!(repo.prune_fts_orphans().unwrap(), 1);
        let ghosts_after: i64 = db.with(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM news_search_fts WHERE news_id NOT IN (SELECT id FROM news_items)",
                [],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(ghosts_after, 0);
    }

    /// Spec §4 line 265：query 英文大小写不敏感。
    /// B4 修复前 normalize_query_text 不做大小写折叠，依赖 FTS unicode61 tokenizer
    /// 隐式 casefold —— 一旦上游 tokenizer 配置改动会立刻泄漏。
    #[test]
    fn query_is_case_insensitive_for_ascii() {
        let db = setup();
        let repo = NewsRepository::new(&db);
        let mut a = sample_item("id-case", "rss:x", None);
        a.title = "gangzi quant terminal".to_string();
        repo.upsert_news_item(&a).unwrap();
        // 全大写 + 单引号 quote 应能命中（normalize 折叠为 lowercase）
        let r = repo
            .list_news_items(None, None, None, Some("GANGZI"), NewsOrder::Desc, 50, 0)
            .unwrap();
        assert_eq!(r.items.len(), 1, "uppercase GANGZI should match lowercase title");
        // 混合大小写
        let r2 = repo
            .list_news_items(None, None, None, Some("Quant"), NewsOrder::Desc, 50, 0)
            .unwrap();
        assert_eq!(r2.items.len(), 1, "mixed-case Quant should match lowercase title");
    }

    /// Spec §4 line 266：多词 query 的 AND / OR / phrase 行为由 News FTS 读模型统一定义，
    /// 不能由调用方各自解释。FTS5 特殊字符必须 strip 掉，避免 query 注入 FTS 运算符。
    /// B5 修复前 normalize 把这些字符原样塞进 MATCH 表达式，会触发 FTS5 语法错误或被
    /// 当成 phrase / NEAR / column-filter 运算符。
    /// Spec §4 line 266：FTS5 特殊字符必须 strip 掉，避免 query 注入 FTS 运算符。
    /// B5 修复前 normalize 把这些字符原样塞进 MATCH 表达式，会触发 FTS5 语法错误或被
    /// 当成 phrase / NEAR / column-filter 运算符。
    /// 本测试关注 **不抛 SQL 错误**；命中语义由 normalize 后剩余的 token 决定。
    #[test]
    fn query_strips_fts5_special_chars_and_does_not_error() {
        let db = setup();
        let repo = NewsRepository::new(&db);
        let mut a = sample_item("id-special", "rss:x", None);
        a.title = "abc widgets".to_string();
        a.summary = Some("brief".to_string());
        repo.upsert_news_item(&a).unwrap();
        // 这些 query 在 B5 修复前会被 FTS5 解读为运算符并抛 syntax error / unknown column。
        for q in [
            "abc + widgets",
            "abc - widgets",
            "abc * widgets",
            "\"abc widgets\"",
            "(abc widgets)",
            "abc:widgets",
            "title:abc",
            "abc OR widgets",
            "abc AND widgets",
            "abc NOT widgets",
        ] {
            let r = repo
                .list_news_items(None, None, None, Some(q), NewsOrder::Desc, 50, 0)
                .expect(&format!("query `{}` must not raise SQL error", q));
            // 仅断言不报错；命中结果集对每个 q 不同
            let _ = r;
        }
        // 显式断言：normalize 之后只剩 `abc widgets` 的 query 必须命中
        let r = repo
            .list_news_items(None, None, None, Some("\"abc widgets\""), NewsOrder::Desc, 50, 0)
            .unwrap();
        assert_eq!(r.items.len(), 1, "stripped quotes still match by AND");
    }

    /// Spec §4：query 仅空白 / 仅特殊字符 → 视为无 query（不应触发 FTS MATCH 引发错误）。
    #[test]
    fn query_only_special_chars_treated_as_empty() {
        let db = setup();
        let repo = NewsRepository::new(&db);
        let mut a = sample_item("id-x", "rss:x", None);
        a.title = "hello".to_string();
        repo.upsert_news_item(&a).unwrap();
        // 仅特殊字符 normalize 为空字符串 → 等价于无 query，应返回所有
        let r = repo
            .list_news_items(None, None, None, Some(" + - * "), NewsOrder::Desc, 50, 0)
            .unwrap();
        assert_eq!(r.items.len(), 1);
    }

    #[test]
    fn normalize_query_text_unit() {
        assert_eq!(normalize_query_text("  Hello   World  "), "hello world");
        assert_eq!(normalize_query_text("ABC+DEF"), "abc def");
        assert_eq!(normalize_query_text("\"phrase\""), "phrase");
        assert_eq!(normalize_query_text("a:b"), "a b");
        assert_eq!(normalize_query_text(" +-* "), "");
        // 中文透传（unicode61 internal casefold 仍然走）
        assert_eq!(normalize_query_text("中文 测试"), "中文 测试");
        // `-` 和 `_` 视为 token 分隔（FTS5 特殊字符 / token 分隔符）
        assert_eq!(normalize_query_text("rss-news_now"), "rss news now");
        // 多种空白折叠
        assert_eq!(normalize_query_text("a\t\nb"), "a b");
    }

    /// Spec §4：fetch_news 支持 query / sources / published_from / published_to 按 AND
    /// 组合。B1 修复前 `fts MATCH ?` placeholder 与其他 clause 的绑定顺序错位，会导致
    /// SQL 报错或返回错误结果。本测试同时启用 4 个条件，覆盖 lockstep clause+binds 装配
    /// 防 B1 回归，并断言 FTS rank 优先级（命中度高的优先）。
    #[test]
    fn list_news_items_with_query_sources_and_time_range() {
        let db = setup();
        let repo = NewsRepository::new(&db);

        // 时间窗口：2026-03-10 .. 2026-03-20
        let in_window_1 = Utc.with_ymd_and_hms(2026, 3, 12, 0, 0, 0).unwrap();
        let in_window_2 = Utc.with_ymd_and_hms(2026, 3, 15, 0, 0, 0).unwrap();
        let before_window = Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap();
        let after_window = Utc.with_ymd_and_hms(2026, 3, 25, 0, 0, 0).unwrap();
        let from = Utc.with_ymd_and_hms(2026, 3, 10, 0, 0, 0).unwrap();
        let to = Utc.with_ymd_and_hms(2026, 3, 20, 0, 0, 0).unwrap();

        // helper: 构造一条 item，定制 source/title/published_at
        let mk = |id: &str, source: &str, title: &str, pub_at: DateTime<Utc>| NewsItem {
            id: id.to_string(),
            source: source.to_string(),
            title: title.to_string(),
            summary: Some("brief".to_string()),
            url: None,
            published_at: Some(pub_at),
            payload: serde_json::json!({}),
            created_at: pub_at,
            updated_at: pub_at,
        };

        // ✅ 全部满足：source=rss:sample, 时间在窗口内, title 命中 "match"
        let target_a = mk("id-target-a", "rss:sample", "match keyword first", in_window_1);
        // ✅ 全部满足，title 含 match 两次（rank 应更高）
        let target_b = mk(
            "id-target-b",
            "rss:sample",
            "match match strong relevance",
            in_window_2,
        );
        // ✗ source 不匹配
        let wrong_source = mk("id-wrong-src", "rss:other", "match keyword", in_window_1);
        // ✗ 时间在窗口前
        let before = mk("id-before", "rss:sample", "match keyword", before_window);
        // ✗ 时间在窗口后
        let after = mk("id-after", "rss:sample", "match keyword", after_window);
        // ✗ title 不命中
        let no_query = mk("id-no-query", "rss:sample", "unrelated topic", in_window_1);

        for it in [
            &target_a,
            &target_b,
            &wrong_source,
            &before,
            &after,
            &no_query,
        ] {
            repo.upsert_news_item(it).unwrap();
        }

        // 4 条件同时启用：query + sources + published_from + published_to
        let r = repo
            .list_news_items(
                Some(&["rss:sample".to_string()]),
                Some(&from),
                Some(&to),
                Some("match"),
                NewsOrder::Desc,
                50,
                0,
            )
            .unwrap();

        // 只命中两条同时满足全部条件的
        assert_eq!(
            r.items.len(),
            2,
            "expected exactly 2 items, got {:?}",
            r.items.iter().map(|i| &i.id).collect::<Vec<_>>()
        );
        let ids: Vec<&str> = r.items.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"id-target-a"));
        assert!(ids.contains(&"id-target-b"));

        // FTS rank：target_b 含两次 "match"，相关性高于 target_a，应排在前面
        // （ORDER BY rank, published_at DESC）。
        assert_eq!(
            r.items[0].id, "id-target-b",
            "higher relevance (more match occurrences) should rank first; got {:?}",
            ids
        );
        assert_eq!(r.items[1].id, "id-target-a");

        // total 与 items 数对齐
        assert_eq!(r.total, 2);
    }

    #[test]
    fn article_upsert_syncs_fts() {
        let db = setup();
        let repo = NewsRepository::new(&db);
        let it = sample_item("id-a", "rss:x", Some("https://a.com/post"));
        repo.upsert_news_item(&it).unwrap();
        let art = ArticleContent {
            url: "https://a.com/post".to_string(),
            first_news_id: Some("id-a".to_string()),
            title: Some("T".to_string()),
            content: Some("the article body about widgets".to_string()),
            payload: serde_json::json!({}),
            fetched_at: Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap(),
            warning: None,
        };
        let out = repo.upsert_article_content(&art).unwrap();
        match out {
            RepoArticleUpsertOutcome::Updated { affected_news_ids } => {
                assert_eq!(affected_news_ids, vec!["id-a".to_string()]);
            }
            _ => panic!("expected Updated"),
        }
        let r = repo
            .list_news_items(None, None, None, Some("widgets"), NewsOrder::Desc, 50, 0)
            .unwrap();
        assert_eq!(r.items.len(), 1);
    }
}
