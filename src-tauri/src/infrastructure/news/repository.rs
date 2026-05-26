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

#[derive(Debug, Default, Clone)]
pub struct NewsSaveDiff {
    pub new_ids: Vec<String>,
    pub updated_ids: Vec<String>,
}

pub fn save_news_items_with_diff(
    app: &AppHandle,
    items: Vec<NewsItem>,
) -> Result<NewsSaveDiff, String> {
    let mut connection = open_database(app)?;
    migrate(&connection)?;
    let tx = connection
        .transaction()
        .map_err(|err| format!("保存资讯缓存失败：{err}"))?;
    let now_ts = now();
    let mut diff = NewsSaveDiff::default();

    for item in items {
        let id = item.id.clone();
        // 先看是否存在
        let existed: Option<String> = tx
            .query_row(
                "select payload_json from news_items where id = ?1",
                params![id],
                |row| row.get::<_, String>(0),
            )
            .ok();
        let source = item.source.clone();
        let published = item.published.clone();
        let payload = serde_json::to_string(&item)
            .map_err(|err| format!("资讯 JSON 序列化失败：{err}"))?;
        let is_new = existed.is_none();
        let is_changed = match &existed {
            Some(prev) => prev != &payload,
            None => true,
        };
        tx.execute(
            "insert into news_items
                (id, source, published, payload_json, created_at, updated_at)
             values (?1, ?2, ?3, ?4, ?5, ?5)
             on conflict(id) do update set
                source = excluded.source,
                published = excluded.published,
                payload_json = excluded.payload_json,
                updated_at = excluded.updated_at",
            params![id, source, published, payload, now_ts],
        )
        .map_err(|err| format!("写入资讯缓存失败：{err}"))?;
        if is_new {
            diff.new_ids.push(id);
        } else if is_changed {
            diff.updated_ids.push(id);
        }
    }

    tx.commit()
        .map_err(|err| format!("提交资讯缓存失败：{err}"))?;
    Ok(diff)
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

/// Spec §4 `fetch_news` 富查询入口：ids / query / sources / published 时间窗口 /
/// limit / offset 组合。返回顺序按 spec：
/// - 有 `query` 时按 FTS relevance（这里降级为 published desc + id asc 稳定排序）
/// - 否则 `published desc, created_at desc, id asc`
/// - `ids` 模式按输入顺序返回
pub fn query_news_items(
    app: &AppHandle,
    opts: NewsQueryOpts,
) -> Result<Vec<NewsItem>, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    let limit = opts.limit.unwrap_or(50).clamp(1, 200);
    let offset = opts.offset.unwrap_or(0).max(0);

    // ids 模式：保持输入顺序，应用其他过滤 + 分页
    if let Some(ids) = &opts.ids {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = (0..ids.len()).map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "select id, payload_json from news_items
             where id in ({placeholders})"
        );
        let mut stmt = connection
            .prepare(&sql)
            .map_err(|e| format!("query_news_items by ids 失败：{e}"))?;
        let map: std::collections::HashMap<String, NewsItem> = stmt
            .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| format!("query_news_items by ids 失败：{e}"))?
            .filter_map(|r| r.ok())
            .filter_map(|(id, payload)| {
                serde_json::from_str::<NewsItem>(&payload).ok().map(|n| (id, n))
            })
            .collect();
        // 按输入顺序
        let ordered: Vec<NewsItem> = ids
            .iter()
            .filter_map(|id| map.get(id).cloned())
            .filter(|item| filter_match(item, &opts))
            .collect();
        let start = (offset as usize).min(ordered.len());
        let end = (start + limit as usize).min(ordered.len());
        return Ok(ordered[start..end].to_vec());
    }

    // 通用查询：直接 build SQL（FTS5 关键字降级为 LIKE，保持稳定排序）
    let mut sql = String::from("select payload_json from news_items where 1=1");
    let mut params_dyn: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(q) = opts.query.as_deref().filter(|s| !s.trim().is_empty()) {
        let pattern = format!(
            "%{}%",
            q.trim().replace('%', "\\%").replace('_', "\\_")
        );
        sql.push_str(" and payload_json like ? escape '\\'");
        params_dyn.push(rusqlite::types::Value::Text(pattern));
    }
    if let Some(sources) = &opts.sources {
        if !sources.is_empty() {
            let ph = (0..sources.len()).map(|_| "?").collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" and source in ({ph})"));
            for s in sources {
                params_dyn.push(rusqlite::types::Value::Text(s.clone()));
            }
        }
    }
    if let Some(from) = &opts.published_from {
        sql.push_str(" and published >= ?");
        params_dyn.push(rusqlite::types::Value::Text(from.clone()));
    }
    if let Some(to) = &opts.published_to {
        sql.push_str(" and published <= ?");
        params_dyn.push(rusqlite::types::Value::Text(to.clone()));
    }
    // spec news-module.md §4：有 query 时按 FTS relevance 排序 + 稳定 tie-breaker；
    // 无 query 时按 publishedAt desc, createdAt desc, id asc。
    if opts.query.as_deref().filter(|s| !s.trim().is_empty()).is_some() {
        // FTS 路径：用 news_fts MATCH + rank（BM25），失败时回退到原 LIKE 路径
        let fts_sql = build_fts_sql(opts.sources.as_ref(), opts.published_from.as_ref(), opts.published_to.as_ref());
        let mut fts_params: Vec<rusqlite::types::Value> = Vec::new();
        fts_params.push(rusqlite::types::Value::Text(
            sanitize_fts_query(opts.query.as_deref().unwrap_or("")),
        ));
        if let Some(sources) = &opts.sources {
            for s in sources {
                fts_params.push(rusqlite::types::Value::Text(s.clone()));
            }
        }
        if let Some(from) = &opts.published_from {
            fts_params.push(rusqlite::types::Value::Text(from.clone()));
        }
        if let Some(to) = &opts.published_to {
            fts_params.push(rusqlite::types::Value::Text(to.clone()));
        }
        fts_params.push(rusqlite::types::Value::Integer(limit));
        fts_params.push(rusqlite::types::Value::Integer(offset));
        if let Ok(rows) = run_fts_query(&connection, &fts_sql, &fts_params) {
            return Ok(rows);
        }
        // FTS 失败 → fallback LIKE path 已包含 query
    }
    sql.push_str(" order by coalesce(published, created_at) desc, id asc limit ? offset ?");
    params_dyn.push(rusqlite::types::Value::Integer(limit));
    params_dyn.push(rusqlite::types::Value::Integer(offset));
    let mut stmt = connection
        .prepare(&sql)
        .map_err(|e| format!("query_news_items 失败：{e}"))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params_dyn.iter()), |row| {
            row.get::<_, String>(0)
        })
        .map_err(|e| format!("query_news_items 失败：{e}"))?
        .filter_map(|r| r.ok())
        .filter_map(|s| serde_json::from_str::<NewsItem>(&s).ok())
        .collect();
    Ok(rows)
}

fn sanitize_fts_query(q: &str) -> String {
    // FTS5 MATCH 字符串：去除双引号 / 控制字符，trim + 折叠空白
    q.replace('"', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn build_fts_sql(
    sources: Option<&Vec<String>>,
    from: Option<&String>,
    to: Option<&String>,
) -> String {
    let mut where_extra = String::new();
    if let Some(srcs) = sources {
        if !srcs.is_empty() {
            let ph = (0..srcs.len())
                .map(|i| format!("?{}", i + 2))
                .collect::<Vec<_>>()
                .join(",");
            where_extra.push_str(&format!(" and ni.source in ({ph})"));
        }
    }
    let mut next_idx = sources.map(|s| s.len()).unwrap_or(0) + 2;
    if from.is_some() {
        where_extra.push_str(&format!(" and ni.published >= ?{next_idx}"));
        next_idx += 1;
    }
    if to.is_some() {
        where_extra.push_str(&format!(" and ni.published <= ?{next_idx}"));
        next_idx += 1;
    }
    let lim_idx = next_idx;
    let off_idx = next_idx + 1;
    format!(
        "select ni.payload_json
         from news_fts f
         join news_items ni on ni.id = f.news_id
         where news_fts match ?1{where_extra}
         order by rank,
                  coalesce(ni.published, ni.created_at) desc,
                  ni.id asc
         limit ?{lim_idx} offset ?{off_idx}"
    )
}

fn run_fts_query(
    connection: &rusqlite::Connection,
    sql: &str,
    params: &[rusqlite::types::Value],
) -> Result<Vec<NewsItem>, String> {
    let mut stmt = connection
        .prepare(sql)
        .map_err(|e| format!("FTS5 prepare 失败：{e}"))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            row.get::<_, String>(0)
        })
        .map_err(|e| format!("FTS5 query 失败：{e}"))?
        .filter_map(|r| r.ok())
        .filter_map(|s| serde_json::from_str::<NewsItem>(&s).ok())
        .collect();
    Ok(rows)
}

fn filter_match(item: &NewsItem, opts: &NewsQueryOpts) -> bool {
    if let Some(sources) = &opts.sources {
        if !sources.is_empty() && !sources.iter().any(|s| s == &item.source) {
            return false;
        }
    }
    if let Some(from) = opts.published_from.as_deref() {
        match item.published.as_deref() {
            Some(p) => {
                if p < from {
                    return false;
                }
            }
            None => return false,
        }
    }
    if let Some(to) = opts.published_to.as_deref() {
        match item.published.as_deref() {
            Some(p) => {
                if p > to {
                    return false;
                }
            }
            None => return false,
        }
    }
    if let Some(q) = opts.query.as_deref().filter(|s| !s.trim().is_empty()) {
        let needle = q.to_lowercase();
        let title_match = item.title.to_lowercase().contains(&needle);
        let summary_match = item
            .summary
            .as_deref()
            .map(|s| s.to_lowercase().contains(&needle))
            .unwrap_or(false);
        if !title_match && !summary_match {
            return false;
        }
    }
    true
}

#[derive(Debug, Default, Clone)]
pub struct NewsQueryOpts {
    pub ids: Option<Vec<String>>,
    pub query: Option<String>,
    pub sources: Option<Vec<String>>,
    pub published_from: Option<String>,
    pub published_to: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub fn load_article_content(app: AppHandle, url: String) -> Result<Option<Value>, String> {
    load_article_content_ref(&app, &url)
}

pub fn load_article_content_ref(app: &AppHandle, url: &str) -> Result<Option<Value>, String> {
    let connection = open_database(app)?;
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

// spec news-module.md §65：News 不按默认保留期主动删除历史。
// `purge_old_news` 已移除（旧 30 天 retention 违反 spec）。如需手动清理由 IPC 显式触发。

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
