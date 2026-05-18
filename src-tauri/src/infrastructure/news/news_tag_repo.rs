//! News tagger 输出落库 + 查询。

use crate::domain::shared::signal::{NewsImportance, NewsKind};
use crate::domain::shared::OccurredAt;
use crate::infrastructure::db::{migrate, open_database};
use rusqlite::{params, OptionalExtension};
use tauri::AppHandle;

#[derive(Debug, Clone)]
pub struct NewsTagsRecord {
    pub news_id: String,
    pub kind: NewsKind,
    pub importance: NewsImportance,
    pub tickers: Vec<String>, // 落库时是 String，避免 StockCode 在 news 处转换错误
    pub sectors: Vec<String>,
    pub tagged_at: OccurredAt,
}

pub fn save(app: &AppHandle, rec: &NewsTagsRecord) -> Result<(), String> {
    let mut conn = open_database(app)?;
    migrate(&conn)?;
    let tx = conn
        .transaction()
        .map_err(|err| format!("开启事务失败：{err}"))?;
    let sectors_json = serde_json::to_string(&rec.sectors)
        .map_err(|err| format!("序列化 sectors 失败：{err}"))?;
    tx.execute(
        "insert or replace into news_tags
            (news_id, kind, importance, sectors, tagged_at)
         values (?1, ?2, ?3, ?4, ?5)",
        params![
            rec.news_id,
            rec.kind.as_str(),
            rec.importance.as_str(),
            sectors_json,
            rec.tagged_at.to_rfc3339(),
        ],
    )
    .map_err(|err| format!("写 news_tags 失败：{err}"))?;
    // tickers 关联表：先删后插（idempotent）
    tx.execute(
        "delete from news_tickers where news_id = ?1",
        params![rec.news_id],
    )
    .map_err(|err| format!("清 news_tickers 失败：{err}"))?;
    for code in &rec.tickers {
        tx.execute(
            "insert or ignore into news_tickers (news_id, code) values (?1, ?2)",
            params![rec.news_id, code],
        )
        .map_err(|err| format!("写 news_tickers 失败：{err}"))?;
    }
    tx.commit()
        .map_err(|err| format!("提交事务失败：{err}"))?;
    Ok(())
}

pub fn get(app: &AppHandle, news_id: &str) -> Result<Option<NewsTagsRecord>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let row = conn
        .query_row(
            "select kind, importance, sectors, tagged_at from news_tags where news_id = ?1",
            params![news_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|err| format!("读 news_tags 失败：{err}"))?;
    let Some((kind_str, importance_str, sectors_json, tagged_at)) = row else {
        return Ok(None);
    };
    let tickers = list_tickers_for_news(&conn, news_id)?;
    let sectors: Vec<String> = match sectors_json {
        Some(j) => serde_json::from_str(&j)
            .map_err(|err| format!("反序列化 sectors 失败：{err}"))?,
        None => Vec::new(),
    };
    Ok(Some(NewsTagsRecord {
        news_id: news_id.to_string(),
        kind: NewsKind::parse(&kind_str).ok_or_else(|| format!("未知 NewsKind: {kind_str}"))?,
        importance: NewsImportance::parse(&importance_str)
            .ok_or_else(|| format!("未知 NewsImportance: {importance_str}"))?,
        tickers,
        sectors,
        tagged_at: parse_occurred(&tagged_at)?,
    }))
}

/// 列出某只股票在 since 之后入库的资讯 news_id 列表。
pub fn list_news_for_code_since(
    app: &AppHandle,
    code: &str,
    since: OccurredAt,
) -> Result<Vec<String>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare(
            "select t.news_id from news_tickers t
             join news_tags ng on ng.news_id = t.news_id
             where t.code = ?1 and ng.tagged_at >= ?2
             order by ng.tagged_at desc",
        )
        .map_err(|err| format!("准备 list_news_for_code_since 失败：{err}"))?;
    let rows = stmt
        .query_map(params![code, since.to_rfc3339()], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|err| format!("query 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect 失败：{err}"))?;
    Ok(rows)
}

fn list_tickers_for_news(conn: &rusqlite::Connection, news_id: &str) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("select code from news_tickers where news_id = ?1")
        .map_err(|err| format!("准备 list_tickers 失败：{err}"))?;
    let result: Vec<String> = stmt
        .query_map(params![news_id], |row| row.get::<_, String>(0))
        .map_err(|err| format!("query tickers 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect tickers 失败：{err}"))?;
    Ok(result)
}

fn parse_occurred(s: &str) -> Result<OccurredAt, String> {
    let dt = chrono::DateTime::parse_from_rfc3339(s)
        .map_err(|err| format!("解析 RFC3339 失败 ({s}): {err}"))?;
    Ok(OccurredAt::new(dt.timestamp_millis()))
}
