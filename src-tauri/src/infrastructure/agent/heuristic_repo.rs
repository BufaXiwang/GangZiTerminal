//! Heuristic aggregate 的 SQLite 持久化。
//!
//! 取代 v2 principle_repo。差异：
//! - 加 supporting_lesson_ids / application_count / hit_count / miss_count / retired_at
//! - origin 接受 seed / user_stated / agent_inferred
//! - effective_state 由调用方派生（不存 state 列）

use crate::domain::agent::heuristic::{
    Heuristic, HeuristicCategory, HeuristicId, HeuristicOrigin,
};
use crate::domain::agent::lesson::LessonId;
use crate::domain::quotes::regime::Regime;
use crate::domain::shared::OccurredAt;
use crate::infrastructure::db::{migrate, open_database};
use rusqlite::{params, OptionalExtension};
use tauri::AppHandle;

pub fn create(app: &AppHandle, h: &Heuristic) -> Result<(), String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let regime_tags_json = serde_json::to_string(
        &h.regime_tags
            .iter()
            .map(|r| r.as_str())
            .collect::<Vec<_>>(),
    )
    .map_err(|err| format!("序列化 regime_tags 失败：{err}"))?;
    let supporting_json = serde_json::to_string(
        &h.supporting_lesson_ids
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>(),
    )
    .map_err(|err| format!("序列化 supporting_lesson_ids 失败：{err}"))?;
    conn.execute(
        "insert into heuristics
            (id, body, category, origin, regime_tags, supporting_lesson_ids,
             application_count, hit_count, miss_count, last_applied_at,
             retired_at, retired_reason, created_at)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            h.id.as_str(),
            h.body,
            h.category.as_str(),
            h.origin.as_str(),
            regime_tags_json,
            supporting_json,
            h.application_count as i64,
            h.hit_count as i64,
            h.miss_count as i64,
            h.last_applied_at.as_ref().map(|o| o.to_rfc3339()),
            h.retired_at.as_ref().map(|o| o.to_rfc3339()),
            h.retired_reason,
            h.created_at.to_rfc3339(),
        ],
    )
    .map_err(|err| format!("插入 heuristic 失败：{err}"))?;
    Ok(())
}

/// 记录一次 application：仅对 origin=agent_inferred 生效（防注水）。
/// outcome=true→hit_count +1；outcome=false→miss_count +1。
pub fn record_application_outcome(
    app: &AppHandle,
    id: &HeuristicId,
    outcome: bool,
    now: OccurredAt,
) -> Result<bool, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let origin: String = conn
        .query_row(
            "select origin from heuristics where id = ?1",
            params![id.as_str()],
            |row| row.get(0),
        )
        .map_err(|err| format!("读 origin 失败：{err}"))?;
    let parsed = HeuristicOrigin::parse(&origin).ok_or_else(|| format!("未知 origin: {origin}"))?;
    if !parsed.allows_system_hit_count() {
        return Ok(false); // user_stated/seed 拒绝自动注水
    }
    let column = if outcome { "hit_count" } else { "miss_count" };
    let sql = format!(
        "update heuristics
         set application_count = application_count + 1,
             {col} = {col} + 1,
             last_applied_at = ?2
         where id = ?1",
        col = column,
    );
    conn.execute(&sql, params![id.as_str(), now.to_rfc3339()])
        .map_err(|err| format!("更新 heuristic outcome 失败：{err}"))?;
    Ok(true)
}

pub fn retire(
    app: &AppHandle,
    id: &HeuristicId,
    reason: String,
    now: OccurredAt,
) -> Result<(), String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    conn.execute(
        "update heuristics
         set retired_at = ?2, retired_reason = ?3
         where id = ?1 and retired_at is null",
        params![id.as_str(), now.to_rfc3339(), reason],
    )
    .map_err(|err| format!("retire heuristic 失败：{err}"))?;
    Ok(())
}

pub fn get(app: &AppHandle, id: &HeuristicId) -> Result<Option<Heuristic>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let row = conn
        .query_row(
            "select body, category, origin, regime_tags, supporting_lesson_ids,
                    application_count, hit_count, miss_count,
                    last_applied_at, retired_at, retired_reason, created_at
             from heuristics where id = ?1",
            params![id.as_str()],
            |row| {
                Ok(HRow {
                    body: row.get(0)?,
                    category: row.get(1)?,
                    origin: row.get(2)?,
                    regime_tags: row.get(3)?,
                    supporting_lesson_ids: row.get(4)?,
                    application_count: row.get(5)?,
                    hit_count: row.get(6)?,
                    miss_count: row.get(7)?,
                    last_applied_at: row.get(8)?,
                    retired_at: row.get(9)?,
                    retired_reason: row.get(10)?,
                    created_at: row.get(11)?,
                })
            },
        )
        .optional()
        .map_err(|err| format!("读取 heuristic 失败：{err}"))?;
    row.map(|r| r.into_domain(id.clone())).transpose()
}

pub fn list_all(app: &AppHandle, limit: i64) -> Result<Vec<Heuristic>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare(
            "select id, body, category, origin, regime_tags, supporting_lesson_ids,
                    application_count, hit_count, miss_count,
                    last_applied_at, retired_at, retired_reason, created_at
             from heuristics
             order by retired_at is null desc, created_at desc limit ?1",
        )
        .map_err(|err| format!("准备 list_all 失败：{err}"))?;
    let rows: Vec<(String, HRow)> = stmt
        .query_map(params![limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                HRow {
                    body: row.get(1)?,
                    category: row.get(2)?,
                    origin: row.get(3)?,
                    regime_tags: row.get(4)?,
                    supporting_lesson_ids: row.get(5)?,
                    application_count: row.get(6)?,
                    hit_count: row.get(7)?,
                    miss_count: row.get(8)?,
                    last_applied_at: row.get(9)?,
                    retired_at: row.get(10)?,
                    retired_reason: row.get(11)?,
                    created_at: row.get(12)?,
                },
            ))
        })
        .map_err(|err| format!("query 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect 失败：{err}"))?;
    let mut out = Vec::with_capacity(rows.len());
    for (id_str, row) in rows {
        out.push(row.into_domain(HeuristicId::from_string(id_str))?);
    }
    Ok(out)
}

/// Prompt 注入候选：未 retired + 按 regime 过滤（regime_tags 空 = 通用） + 按 effective_state 排序。
/// 上限 25 条（v3 spec § 5）。
pub fn list_for_prompt(
    app: &AppHandle,
    current_regime: Option<Regime>,
    limit: i64,
) -> Result<Vec<Heuristic>, String> {
    let all = list_all(app, 1000)?;
    let filtered: Vec<Heuristic> = all
        .into_iter()
        .filter(|h| h.retired_at.is_none())
        .filter(|h| h.is_promptable())
        .filter(|h| {
            if h.regime_tags.is_empty() {
                return true;
            }
            let Some(reg) = current_regime else {
                return true;
            };
            h.regime_tags.contains(&reg)
        })
        .take(limit as usize)
        .collect();
    Ok(filtered)
}

pub fn count_by_state(app: &AppHandle) -> Result<HeuristicCounts, String> {
    let all = list_all(app, 10_000)?;
    let mut c = HeuristicCounts::default();
    for h in all {
        match h.origin {
            HeuristicOrigin::Seed => c.seed += 1,
            HeuristicOrigin::UserStated => c.user_stated += 1,
            HeuristicOrigin::AgentInferred => c.agent_inferred += 1,
        }
        if h.retired_at.is_some() {
            c.retired += 1;
        }
    }
    Ok(c)
}

#[derive(Debug, Clone, Default)]
pub struct HeuristicCounts {
    pub seed: u32,
    pub user_stated: u32,
    pub agent_inferred: u32,
    pub retired: u32,
}

// ====== 内部 ============================================================

struct HRow {
    body: String,
    category: String,
    origin: String,
    regime_tags: Option<String>,
    supporting_lesson_ids: Option<String>,
    application_count: u32,
    hit_count: u32,
    miss_count: u32,
    last_applied_at: Option<String>,
    retired_at: Option<String>,
    retired_reason: Option<String>,
    created_at: String,
}

impl HRow {
    fn into_domain(self, id: HeuristicId) -> Result<Heuristic, String> {
        let category = HeuristicCategory::parse(&self.category)
            .ok_or_else(|| format!("未知 category: {}", self.category))?;
        let origin = HeuristicOrigin::parse(&self.origin)
            .ok_or_else(|| format!("未知 origin: {}", self.origin))?;
        let regime_tags: Vec<Regime> = match self.regime_tags {
            Some(j) => {
                let tags: Vec<String> = serde_json::from_str(&j)
                    .map_err(|err| format!("反序列化 regime_tags 失败：{err}"))?;
                tags.iter().filter_map(|t| Regime::parse(t)).collect()
            }
            None => Vec::new(),
        };
        let supporting_lesson_ids: Vec<LessonId> = match self.supporting_lesson_ids {
            Some(j) => {
                let ids: Vec<String> = serde_json::from_str(&j)
                    .map_err(|err| format!("反序列化 supporting_lesson_ids 失败：{err}"))?;
                ids.into_iter().map(LessonId::from_string).collect()
            }
            None => Vec::new(),
        };
        Ok(Heuristic {
            id,
            body: self.body,
            category,
            origin,
            regime_tags,
            supporting_lesson_ids,
            application_count: self.application_count,
            hit_count: self.hit_count,
            miss_count: self.miss_count,
            last_applied_at: match self.last_applied_at {
                Some(s) => Some(parse_occurred(&s)?),
                None => None,
            },
            retired_at: match self.retired_at {
                Some(s) => Some(parse_occurred(&s)?),
                None => None,
            },
            retired_reason: self.retired_reason,
            created_at: parse_occurred(&self.created_at)?,
        })
    }
}

fn parse_occurred(s: &str) -> Result<OccurredAt, String> {
    let dt = chrono::DateTime::parse_from_rfc3339(s)
        .map_err(|err| format!("解析 RFC3339 失败 ({s}): {err}"))?;
    Ok(OccurredAt::new(dt.timestamp_millis()))
}
