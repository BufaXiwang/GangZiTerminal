//! News 子域 DB 访问——news_items + article_contents 表的 CRUD。
//!
//! News 模块只提供"拉取 + 存储 + 查询"：分析消费状态由 Agent BC 自管
//! （见 `infrastructure/agent/news_analysis_repo.rs`）。本文件不感知任何
//! 下游消费者，只负责基础数据层。

use crate::domain::news::NewsItem;
use crate::infrastructure::db::{json_string, migrate, now, open_database, required_json_string};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use tauri::AppHandle;

pub fn list_news_items(app: AppHandle, limit: Option<i64>) -> Result<Vec<NewsItem>, String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let mut statement = connection
        .prepare(
            "select payload_json
             from news_items
             order by coalesce(published, updated_at) desc
             limit ?1",
        )
        .map_err(|err| format!("读取资讯缓存失败：{err}"))?;
    let items = statement
        .query_map(params![limit.unwrap_or(300).clamp(1, 1000)], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|err| format!("读取资讯缓存失败：{err}"))?
        .map(|raw| {
            raw.map_err(|err| format!("读取资讯缓存失败：{err}"))
                .and_then(|payload| {
                    serde_json::from_str::<NewsItem>(&payload)
                        .map_err(|err| format!("资讯 JSON 解析失败：{err}"))
                })
        })
        .collect();
    items
}

/// 入库——重复 id 走 upsert 更新内容。
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
        let payload = serde_json::to_string(&item)
            .map_err(|err| format!("资讯 JSON 序列化失败：{err}"))?;
        tx.execute(
            "insert into news_items
                (id, source, published, payload_json, created_at, updated_at)
             values (?1, ?2, ?3, ?4, ?5, ?5)
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
        "select payload_json from news_items
         where id in ({})
         order by coalesce(published, updated_at) desc",
        placeholders
    );
    let mut stmt = connection
        .prepare(&sql)
        .map_err(|err| format!("查询资讯失败：{err}"))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
            row.get::<_, String>(0)
        })
        .map_err(|err| format!("查询资讯失败：{err}"))?
        .map(|raw| {
            raw.map_err(|err| format!("查询资讯失败：{err}"))
                .and_then(|payload| {
                    serde_json::from_str::<NewsItem>(&payload)
                        .map_err(|err| format!("资讯 JSON 解析失败：{err}"))
                })
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
            "select ni.payload_json
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
        .query_map(params![pattern, limit], |row| row.get::<_, String>(0))
        .map_err(|err| format!("FTS5 查询资讯失败：{err}"))?
        .map(|raw| {
            raw.map_err(|err| format!("FTS5 查询资讯失败：{err}"))
                .and_then(|payload| {
                    serde_json::from_str::<NewsItem>(&payload)
                        .map_err(|err| format!("资讯 JSON 解析失败：{err}"))
                })
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
            "select payload_json from news_items
             where payload_json like ?1 escape '\\'
             order by coalesce(published, updated_at) desc
             limit ?2",
        )
        .map_err(|err| format!("LIKE 查询资讯失败：{err}"))?;
    let rows = stmt
        .query_map(params![pattern, limit], |row| row.get::<_, String>(0))
        .map_err(|err| format!("LIKE 查询资讯失败：{err}"))?
        .map(|raw| {
            raw.map_err(|err| format!("LIKE 查询资讯失败：{err}"))
                .and_then(|payload| {
                    serde_json::from_str::<NewsItem>(&payload)
                        .map_err(|err| format!("资讯 JSON 解析失败：{err}"))
                })
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

/// 删除 `published` 早于 cutoff（RFC3339）的 news_items + 级联清 article_contents 孤儿。
/// 同时清 agent_news_analysis_state 表里指向已删 news 的孤儿（防止该表无限增长）。
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
    let _ = connection.execute(
        "delete from agent_news_analysis_state
         where news_id not in (select id from news_items)",
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
