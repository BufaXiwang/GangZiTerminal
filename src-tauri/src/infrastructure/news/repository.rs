//! News 子域 DB 访问——news_items + article_contents 表的 CRUD。
//!
//! 表设计：
//! - `news_items`：资讯条目（id PK / source / published / payload_json）
//! - `article_contents`：文章正文缓存（url PK / item_id / payload_json）
//!
//! 写路径：scheduler::news_refresh_loop 周期调 save_news_items；fetch_article_content 调 save_article_content。
//! 读路径：list/get 给前端 + agent SearchNewsTool 用。

use crate::infrastructure::db::{json_string, migrate, now, open_database, required_json_string};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use tauri::AppHandle;

#[tauri::command]
pub fn list_news_items(app: AppHandle, limit: Option<i64>) -> Result<Vec<Value>, String> {
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
                    serde_json::from_str::<Value>(&payload)
                        .map_err(|err| format!("资讯 JSON 解析失败：{err}"))
                })
        })
        .collect();
    items
}

pub fn save_news_items(app: AppHandle, items: Vec<Value>) -> Result<usize, String> {
    let mut connection = open_database(&app)?;
    migrate(&connection)?;
    let tx = connection
        .transaction()
        .map_err(|err| format!("保存资讯缓存失败：{err}"))?;
    let now = now();
    let mut saved = 0usize;

    for item in items {
        let id = required_json_string(&item, "/id", "资讯缺少 id")?;
        let source = required_json_string(&item, "/source", "资讯缺少 source")?;
        let published = json_string(&item, "/published");
        // 冲突时不覆盖 analysis_status，保留旧的分析状态
        tx.execute(
            "insert into news_items (id, source, published, payload_json, created_at, updated_at)
             values (?1, ?2, ?3, ?4, ?5, ?5)
             on conflict(id) do update set
                source = excluded.source,
                published = excluded.published,
                payload_json = excluded.payload_json,
                updated_at = excluded.updated_at",
            params![id, source, published, item.to_string(), now],
        )
        .map_err(|err| format!("写入资讯缓存失败：{err}"))?;
        saved += 1;
    }

    tx.commit()
        .map_err(|err| format!("提交资讯缓存失败：{err}"))?;
    Ok(saved)
}

#[tauri::command]
pub fn get_news_items_by_ids(app: AppHandle, ids: Vec<String>) -> Result<Vec<Value>, String> {
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
                    serde_json::from_str::<Value>(&payload)
                        .map_err(|err| format!("资讯 JSON 解析失败：{err}"))
                })
        })
        .collect::<Result<Vec<Value>, String>>()?;
    Ok(rows)
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
