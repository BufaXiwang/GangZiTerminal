//! News 子域 DB 访问——news_items + article_contents 表的 CRUD。
//!
//! 表设计：
//! - `news_items`：资讯条目（id PK / source / published / payload_json）
//! - `article_contents`：文章正文缓存（url PK / item_id / payload_json）
//!
//! 写路径：scheduler::news_refresh_loop 周期调 save_news_items；fetch_article_content 调 save_article_content。
//! 读路径：list/get/search 给 adapter + agent SearchNewsTool 用。

use crate::domain::news::{NewsItem, NewsStatus};
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
        tx.execute(
            "insert into news_items (id, source, published, payload_json, created_at, updated_at)
             values (?1, ?2, ?3, ?4, ?5, ?5)
             on conflict(id) do update set
                source = excluded.source,
                published = excluded.published,
                payload_json = excluded.payload_json,
                updated_at = excluded.updated_at",
            params![
                id,
                source,
                published,
                serde_json::to_string(&item)
                    .map_err(|err| format!("资讯 JSON 序列化失败：{err}"))?,
                now
            ],
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
        "select payload_json from news_items where id in ({}) order by coalesce(published, updated_at) desc",
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

    // 优先走 FTS5——trigram tokenizer 对中文按 3 字符窗口索引，性能 + 召回都比
    // payload_json LIKE '%q%' 强一大截。FTS 失败（极少数情况：query 含 FTS 元字符
    // 触发 syntax error）时静默回退到老 LIKE 路径，保证可用性。
    if let Ok(rows) = search_news_items_fts(&connection, q, lim) {
        return Ok(rows);
    }
    search_news_items_like(&connection, q, lim)
}

/// 基于 news_fts 的 LIKE 搜索——配合 trigram 索引，**任意长度**查询都被加速。
///
/// 为什么不用 MATCH：trigram tokenizer 的 MATCH 严格要求 3+ 字符短语，"美股"
/// "央行" "白酒" 等 2 字常见中文短词全部 0 命中。SQLite 官方文档点明 trigram
/// 同时加速 LIKE / GLOB——LIKE '%X%' 内部用 trigram 索引精准跳跃，无需全表扫。
/// 我们牺牲 BM25 排序换通用性：按 published desc 排（新闻场景下时间优先于相关度）。
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

/// 老 LIKE 回退——payload_json 整字段子串匹配。FTS 路径失败时兜底。
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

#[allow(dead_code)]
pub fn claim_pending(app: AppHandle, ids: &[String]) -> Result<usize, String> {
    transition_news_items(app, ids, NewsStatus::Processing)
}

#[allow(dead_code)]
pub fn mark_consumed(app: AppHandle, ids: &[String]) -> Result<usize, String> {
    transition_news_items(app, ids, NewsStatus::Consumed)
}

#[allow(dead_code)]
pub fn revert_claim(app: AppHandle, ids: &[String]) -> Result<usize, String> {
    transition_news_items(app, ids, NewsStatus::Pending)
}

#[allow(dead_code)]
pub fn mark_failed(app: AppHandle, ids: &[String]) -> Result<usize, String> {
    transition_news_items(app, ids, NewsStatus::Failed)
}

#[allow(dead_code)]
fn transition_news_items(
    app: AppHandle,
    ids: &[String],
    next: NewsStatus,
) -> Result<usize, String> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut connection = open_database(&app)?;
    migrate(&connection)?;
    let tx = connection
        .transaction()
        .map_err(|err| format!("更新资讯状态失败：{err}"))?;
    let now = now();
    let mut changed = 0usize;

    for id in ids {
        let raw = tx
            .query_row(
                "select payload_json from news_items where id = ?1",
                params![id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|err| format!("读取资讯状态失败：{err}"))?;
        let Some(raw) = raw else {
            continue;
        };
        let mut item: NewsItem =
            serde_json::from_str(&raw).map_err(|err| format!("资讯 JSON 解析失败：{err}"))?;
        item.transition_to(next).map_err(|err| err.to_string())?;
        tx.execute(
            "update news_items
             set payload_json = ?2, updated_at = ?3
             where id = ?1",
            params![
                id,
                serde_json::to_string(&item)
                    .map_err(|err| format!("资讯 JSON 序列化失败：{err}"))?,
                now
            ],
        )
        .map_err(|err| format!("更新资讯状态失败：{err}"))?;
        changed += 1;
    }

    tx.commit()
        .map_err(|err| format!("提交资讯状态失败：{err}"))?;
    Ok(changed)
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

/// 删除 `published` 字段早于 cutoff（RFC3339 字符串）的 news_items + 级联清孤儿。
///
/// `published` 是 RSS/NewsNow 给的发布时间；`updated_at` 是入库时间。优先按 `published`
/// 卡，没值的回落到 `updated_at`——保证早期没 published 字段的源也会被清。
///
/// 同时清理：
/// - news_tags / news_tickers 中孤儿（关联的 news_id 已不存在）
/// - article_contents 中孤儿（item_id 不在 news_items）
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
    // 级联：tags / tickers / article_contents 中孤儿
    let _ = connection.execute(
        "delete from news_tags where news_id not in (select id from news_items)",
        [],
    );
    let _ = connection.execute(
        "delete from news_tickers where news_id not in (select id from news_items)",
        [],
    );
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
    let fetched_at = json_string(&article, "/fetchedAt").unwrap_or_else(|| now());
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
