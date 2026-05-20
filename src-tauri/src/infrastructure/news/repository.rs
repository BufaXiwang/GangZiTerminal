//! News 子域 DB 访问——news_items + article_contents 表的 CRUD。
//!
//! 表设计（v8）：
//! - `news_items`：资讯条目（id PK / source / published / **analysis_status 行级列** / processing_started_at / payload_json）
//! - `article_contents`：文章正文缓存（url PK / item_id / payload_json）
//!
//! analysis_status 提升到行级列后，并发批量 claim 由 `infrastructure/news/batch.rs` 处理
//! （UPDATE ... RETURNING 单语句原子化）。本文件只管基础 CRUD + search。
//!
//! 写路径：scheduler::news_refresh_loop 周期调 save_news_items；fetch_article_content 调 save_article_content。
//! 读路径：list/get/search 给 adapter + agent SearchNewsTool 用——读取时把 column 里的 status 填回 NewsItem。

use crate::domain::news::{NewsItem, NewsStatus};
use crate::infrastructure::db::{json_string, migrate, now, open_database, required_json_string};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use tauri::AppHandle;

/// 行级 status 反序列化。
fn parse_status(s: &str) -> NewsStatus {
    match s {
        "processing" => NewsStatus::Processing,
        "consumed" => NewsStatus::Consumed,
        "failed" => NewsStatus::Failed,
        _ => NewsStatus::Pending,
    }
}

/// 从 (payload_json, analysis_status) 重建 NewsItem——把行级 status 填回 domain 字段。
fn hydrate(payload: &str, status_col: &str) -> Result<NewsItem, String> {
    let mut item: NewsItem =
        serde_json::from_str(payload).map_err(|err| format!("资讯 JSON 解析失败：{err}"))?;
    item.analysis_status = Some(parse_status(status_col));
    Ok(item)
}

pub fn list_news_items(app: AppHandle, limit: Option<i64>) -> Result<Vec<NewsItem>, String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let mut statement = connection
        .prepare(
            "select payload_json, analysis_status
             from news_items
             order by coalesce(published, updated_at) desc
             limit ?1",
        )
        .map_err(|err| format!("读取资讯缓存失败：{err}"))?;
    let items = statement
        .query_map(params![limit.unwrap_or(300).clamp(1, 1000)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|err| format!("读取资讯缓存失败：{err}"))?
        .map(|raw| {
            raw.map_err(|err| format!("读取资讯缓存失败：{err}"))
                .and_then(|(payload, status)| hydrate(&payload, &status))
        })
        .collect();
    items
}

/// 入库——默认 analysis_status='pending'，重复 id 走 upsert 更新内容但**不重置 status**。
pub fn save_news_items(app: AppHandle, items: Vec<NewsItem>) -> Result<usize, String> {
    let mut connection = open_database(&app)?;
    migrate(&connection)?;
    let tx = connection
        .transaction()
        .map_err(|err| format!("保存资讯缓存失败：{err}"))?;
    let now = now();
    let mut saved = 0usize;

    for item in items {
        let id = item.id.clone();
        let source = item.source.clone();
        let published = item.published.clone();
        // 序列化时清掉 analysis_status——避免 JSON 与行级列两处不一致。读时从列恢复。
        let mut payload_item = item.clone();
        payload_item.analysis_status = None;
        let payload = serde_json::to_string(&payload_item)
            .map_err(|err| format!("资讯 JSON 序列化失败：{err}"))?;
        tx.execute(
            "insert into news_items
                (id, source, published, analysis_status, payload_json, created_at, updated_at)
             values (?1, ?2, ?3, 'pending', ?4, ?5, ?5)
             on conflict(id) do update set
                source = excluded.source,
                published = excluded.published,
                payload_json = excluded.payload_json,
                updated_at = excluded.updated_at",
            params![id, source, published, payload, now],
        )
        .map_err(|err| format!("写入资讯缓存失败：{err}"))?;
        saved += 1;
    }

    tx.commit()
        .map_err(|err| format!("提交资讯缓存失败：{err}"))?;
    Ok(saved)
}

pub fn get_news_items_by_ids(app: AppHandle, ids: Vec<String>) -> Result<Vec<NewsItem>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let placeholders = (0..ids.len()).map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "select payload_json, analysis_status from news_items
         where id in ({})
         order by coalesce(published, updated_at) desc",
        placeholders
    );
    let mut stmt = connection
        .prepare(&sql)
        .map_err(|err| format!("查询资讯失败：{err}"))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|err| format!("查询资讯失败：{err}"))?
        .map(|raw| {
            raw.map_err(|err| format!("查询资讯失败：{err}"))
                .and_then(|(payload, status)| hydrate(&payload, &status))
        })
        .collect::<Result<Vec<NewsItem>, String>>()?;
    Ok(rows)
}

pub fn search_news_items(
    app: AppHandle,
    query: String,
    limit: Option<i64>,
) -> Result<Vec<NewsItem>, String> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let lim = limit.unwrap_or(20).clamp(1, 50);

    // 优先 FTS5（trigram tokenizer 加速中文 LIKE），失败回退 payload LIKE
    if let Ok(rows) = search_news_items_fts(&connection, q, lim) {
        return Ok(rows);
    }
    search_news_items_like(&connection, q, lim)
}

fn search_news_items_fts(
    connection: &rusqlite::Connection,
    query: &str,
    limit: i64,
) -> Result<Vec<NewsItem>, String> {
    let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
    let mut stmt = connection
        .prepare(
            "select ni.payload_json, ni.analysis_status
             from news_fts f
             join news_items ni on ni.id = f.news_id
             where f.title like ?1 escape '\\'
                or f.summary like ?1 escape '\\'
                or f.source like ?1 escape '\\'
             order by coalesce(ni.published, ni.updated_at) desc
             limit ?2",
        )
        .map_err(|err| format!("FTS5 查询资讯失败：{err}"))?;
    let rows = stmt
        .query_map(params![pattern, limit], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|err| format!("FTS5 查询资讯失败：{err}"))?
        .map(|raw| {
            raw.map_err(|err| format!("FTS5 查询资讯失败：{err}"))
                .and_then(|(payload, status)| hydrate(&payload, &status))
        })
        .collect();
    rows
}

fn search_news_items_like(
    connection: &rusqlite::Connection,
    query: &str,
    limit: i64,
) -> Result<Vec<NewsItem>, String> {
    let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
    let mut stmt = connection
        .prepare(
            "select payload_json, analysis_status from news_items
             where payload_json like ?1 escape '\\'
             order by coalesce(published, updated_at) desc
             limit ?2",
        )
        .map_err(|err| format!("LIKE 查询资讯失败：{err}"))?;
    let rows = stmt
        .query_map(params![pattern, limit], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|err| format!("LIKE 查询资讯失败：{err}"))?
        .map(|raw| {
            raw.map_err(|err| format!("LIKE 查询资讯失败：{err}"))
                .and_then(|(payload, status)| hydrate(&payload, &status))
        })
        .collect();
    rows
}

pub fn load_article_content(app: AppHandle, url: String) -> Result<Option<Value>, String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let raw = connection
        .query_row(
            "select payload_json from article_contents where url = ?1",
            params![url],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|err| format!("读取正文缓存失败：{err}"))?;

    raw.map(|text| {
        serde_json::from_str(&text).map_err(|err| format!("正文缓存 JSON 解析失败：{err}"))
    })
    .transpose()
}

/// 删除 `published` 字段早于 cutoff（RFC3339）的 news_items + 级联清 article_contents 孤儿。
pub fn purge_old_news(app: &AppHandle, cutoff_rfc3339: &str) -> Result<u64, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    let deleted_items: usize = connection
        .execute(
            "delete from news_items
             where coalesce(published, updated_at) < ?1",
            params![cutoff_rfc3339],
        )
        .map_err(|err| format!("清理旧资讯失败：{err}"))?;
    let _ = connection.execute(
        "delete from article_contents
         where item_id is not null and item_id not in (select id from news_items)",
        [],
    );
    Ok(deleted_items as u64)
}

pub fn save_article_content(
    app: AppHandle,
    item_id: Option<String>,
    article: Value,
) -> Result<(), String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let url = required_json_string(&article, "/url", "正文缓存缺少 url")?;
    if url.trim().is_empty() {
        return Ok(());
    }
    let fetched_at = json_string(&article, "/fetchedAt").unwrap_or_else(now);
    connection
        .execute(
            "insert into article_contents (url, item_id, payload_json, fetched_at)
             values (?1, ?2, ?3, ?4)
             on conflict(url) do update set
                item_id = excluded.item_id,
                payload_json = excluded.payload_json,
                fetched_at = excluded.fetched_at",
            params![url, item_id, article.to_string(), fetched_at],
        )
        .map_err(|err| format!("写入正文缓存失败：{err}"))?;
    Ok(())
}
