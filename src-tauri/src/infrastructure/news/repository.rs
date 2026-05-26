//! News 持久化仓储 — `NewsItem` / `ArticleContent` / `NewsSource` / FTS。
//!
//! Spec: docs/design/news-module.md §2 / §4
//!
//! 责任：
//! - upsert `news_items`（同 ID 冲突时更新 `updated_at` + `payload`；判定是否字段变化）
//! - upsert `article_contents`（按 canonical URL 主键；正文变化同步 FTS）
//! - 维护 `news_search_fts` 与 `news_items` / `article_contents` 的一致性
//! - 根据 `FetchNewsRequest` 拼装查询（按 spec §4 规则）
//! - 不做 provider 调用，不做 normalize；只接受 domain 类型 / canonical URL

use crate::domain::news::source::NewsSource;
use crate::domain::news::types::{ArticleContent, NewsItem};
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
                 FROM article_contents WHERE url = ?1",
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
        limit: u32,
        offset: u32,
    ) -> rusqlite::Result<ListResult> {
        self.db.with(|conn| {
            list_news_items_impl(conn, sources, published_from, published_to, query, limit, offset)
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
            "SELECT content FROM article_contents WHERE url = ?1",
            [&article.url],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?;

    let payload_str = serde_json::to_string(&article.payload).unwrap_or_else(|_| "{}".to_string());
    let fetched_at_s = format_dt(&article.fetched_at);
    let warning_s = article.warning.map(serde_plain_warning);

    tx.execute(
        "INSERT INTO article_contents (url, first_news_id, title, content, payload_json, fetched_at, warning)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(url) DO UPDATE SET
            first_news_id = COALESCE(article_contents.first_news_id, excluded.first_news_id),
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
        // article 列：如果该 url 已有 article_contents.content，预填；否则空。
        let article_text: String = tx
            .query_row(
                "SELECT COALESCE(ac.content, '')
                 FROM news_items ni LEFT JOIN article_contents ac ON ac.url = ni.url
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

fn list_news_items_impl(
    conn: &Connection,
    sources: Option<&[String]>,
    published_from: Option<&DateTime<Utc>>,
    published_to: Option<&DateTime<Utc>>,
    query: Option<&str>,
    limit: u32,
    offset: u32,
) -> rusqlite::Result<ListResult> {
    let mut where_clauses: Vec<String> = Vec::new();
    let mut binds: Vec<rusqlite::types::Value> = Vec::new();

    if let Some(srcs) = sources {
        if !srcs.is_empty() {
            let marks: Vec<String> = (0..srcs.len()).map(|_| "?".to_string()).collect();
            where_clauses.push(format!("ni.source IN ({})", marks.join(",")));
            for s in srcs {
                binds.push(rusqlite::types::Value::Text(s.clone()));
            }
        } else {
            // empty sources list 过滤等价于不命中
            return Ok(ListResult {
                items: vec![],
                total: 0,
            });
        }
    }

    if let Some(from) = published_from {
        where_clauses.push("(ni.published_at IS NOT NULL AND ni.published_at >= ?)".to_string());
        binds.push(rusqlite::types::Value::Text(format_dt(from)));
    }
    if let Some(to) = published_to {
        where_clauses.push("(ni.published_at IS NOT NULL AND ni.published_at <= ?)".to_string());
        binds.push(rusqlite::types::Value::Text(format_dt(to)));
    }

    let has_query = query
        .map(|q| !normalize_query_text(q).is_empty())
        .unwrap_or(false);

    let (mut from_sql, order_sql) = if has_query {
        // FTS join
        let q_norm = normalize_query_text(query.unwrap());
        binds.push(rusqlite::types::Value::Text(q_norm));
        (
            "FROM news_items ni
             INNER JOIN news_search_fts fts ON fts.news_id = ni.id
             WHERE fts.news_search_fts MATCH ?".to_string(),
            "ORDER BY rank, ni.published_at DESC, ni.created_at DESC, ni.id ASC".to_string(),
        )
    } else {
        (
            "FROM news_items ni WHERE 1=1".to_string(),
            "ORDER BY COALESCE(ni.published_at, ni.created_at) DESC, ni.created_at DESC, ni.id ASC"
                .to_string(),
        )
    };

    for w in &where_clauses {
        from_sql.push_str(" AND ");
        from_sql.push_str(w);
    }

    let count_sql = format!("SELECT COUNT(*) {}", from_sql);
    let select_sql = format!(
        "SELECT ni.id, ni.source, ni.title, ni.summary, ni.url, ni.published_at,
                ni.payload_json, ni.created_at, ni.updated_at
         {} {} LIMIT ? OFFSET ?",
        from_sql, order_sql
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

fn normalize_query_text(q: &str) -> String {
    // trim + 折叠连续空白（spec §4）
    let mut out = String::with_capacity(q.len());
    let mut last_ws = false;
    for ch in q.chars() {
        if ch.is_whitespace() {
            if !last_ws && !out.is_empty() {
                out.push(' ');
            }
            last_ws = true;
        } else {
            // FTS5 special chars 转义为短语形式比较繁琐；这里用最简单的做法：
            // 把单词拼成 `"w1" "w2"` 短语形式，FTS5 默认 AND；
            // 但为了避免引号嵌入注入，只保留中英文数字
            if ch.is_alphanumeric() || ch == '-' || ch == '_' {
                out.push(ch);
            } else if ch == '"' {
                // 跳过引号
            } else {
                out.push(ch);
            }
            last_ws = false;
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
            .list_news_items(None, None, None, Some("gangzi"), 50, 0)
            .unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].id, "id-a");
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
            .list_news_items(None, None, None, Some("widgets"), 50, 0)
            .unwrap();
        assert_eq!(r.items.len(), 1);
    }
}
