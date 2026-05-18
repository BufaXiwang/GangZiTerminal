//! Principle aggregate 的 SQLite 持久化（归属 agent BC）。
//!
//! API：
//! - 写：create / update_state / increment_hit / set_last_applied
//! - 读：get / list_by_state / list_for_prompt（按 hit_count + regime 过滤 top-N）
//! - 派生指标：state 流动性 / origin 分布（health_metrics 用）

use crate::domain::agent::principle::{
    Principle, PrincipleCategory, PrincipleId, PrincipleOrigin, PrincipleState,
};
use crate::domain::quotes::regime::Regime;
use crate::domain::shared::OccurredAt;
use crate::infrastructure::db::{migrate, open_database};
use rusqlite::{params, OptionalExtension};
use tauri::AppHandle;

// ====== 写入 ============================================================

pub fn create_principle(app: &AppHandle, p: &Principle) -> Result<(), String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let regime_tags_json = serde_json::to_string(
        &p.regime_tags
            .iter()
            .map(|r| r.as_str())
            .collect::<Vec<_>>(),
    )
    .map_err(|err| format!("序列化 regime_tags 失败：{err}"))?;
    conn.execute(
        "insert into principles
            (id, body, category, origin, state, regime_tags, hit_count, last_applied_at, created_at)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            p.id.as_str(),
            p.body,
            p.category.as_str(),
            p.origin.as_str(),
            p.state.as_str(),
            regime_tags_json,
            p.hit_count,
            p.last_applied_at.as_ref().map(|o| o.to_rfc3339()),
            p.created_at.to_rfc3339(),
        ],
    )
    .map_err(|err| format!("插入 principle 失败：{err}"))?;
    Ok(())
}

pub fn update_principle_state(
    app: &AppHandle,
    id: &PrincipleId,
    new_state: PrincipleState,
) -> Result<(), String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    conn.execute(
        "update principles set state = ?2 where id = ?1",
        params![id.as_str(), new_state.as_str()],
    )
    .map_err(|err| format!("更新 principle state 失败：{err}"))?;
    Ok(())
}

/// hit_count +1 并刷新 last_applied_at。
/// 调用方必须自己检查 origin/state——本函数无业务校验，纯 SQL。
/// 业务规则（防 user_stated 被 agent 自加 hit_count）在 pipeline/adapter 层做。
pub fn increment_hit(
    app: &AppHandle,
    id: &PrincipleId,
    now_occurred: OccurredAt,
) -> Result<(), String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    conn.execute(
        "update principles
         set hit_count = hit_count + 1,
             last_applied_at = ?2
         where id = ?1",
        params![id.as_str(), now_occurred.to_rfc3339()],
    )
    .map_err(|err| format!("递增 hit_count 失败：{err}"))?;
    Ok(())
}

// ====== 读取 ============================================================

pub fn get_principle(app: &AppHandle, id: &PrincipleId) -> Result<Option<Principle>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let row = conn
        .query_row(
            "select body, category, origin, state, regime_tags, hit_count,
                    last_applied_at, created_at
             from principles where id = ?1",
            params![id.as_str()],
            |row| {
                Ok(PrincipleRow {
                    body: row.get(0)?,
                    category: row.get(1)?,
                    origin: row.get(2)?,
                    state: row.get(3)?,
                    regime_tags: row.get(4)?,
                    hit_count: row.get(5)?,
                    last_applied_at: row.get(6)?,
                    created_at: row.get(7)?,
                })
            },
        )
        .optional()
        .map_err(|err| format!("读取 principle 失败：{err}"))?;
    let Some(row) = row else { return Ok(None) };
    Ok(Some(row.into_domain(id.clone())?))
}

pub fn list_by_state(
    app: &AppHandle,
    state: PrincipleState,
    limit: i64,
) -> Result<Vec<Principle>, String> {
    list_internal(app, Some(state), None, limit)
}

pub fn list_all(app: &AppHandle, limit: i64) -> Result<Vec<Principle>, String> {
    list_internal(app, None, None, limit)
}

/// Prompt 注入候选池：state=Active + 按 regime 过滤（regime_tags 空 = 通用）+ 按 hit_count 降序。
/// 取 top `limit`（spec § 5.2 默认 25 条上限）。
pub fn list_for_prompt(
    app: &AppHandle,
    current_regime: Option<Regime>,
    limit: i64,
) -> Result<Vec<Principle>, String> {
    let all_active = list_internal(app, Some(PrincipleState::Active), None, 1000)?;
    let filtered: Vec<Principle> = all_active
        .into_iter()
        .filter(|p| {
            // 空 regime_tags = 通用，永远通过
            if p.regime_tags.is_empty() {
                return true;
            }
            // 不知道当前 regime → 不过滤（保守）
            let Some(reg) = current_regime else {
                return true;
            };
            p.regime_tags.contains(&reg)
        })
        .take(limit as usize)
        .collect();
    Ok(filtered)
}

/// 派生统计——供 W3-2 health metrics 用。
#[derive(Debug, Clone)]
pub struct PrincipleCounts {
    pub proposed: u32,
    pub active: u32,
    pub dormant: u32,
    pub retired: u32,
    pub user_stated: u32,
    pub agent_inferred: u32,
}

pub fn count_by_state_and_origin(app: &AppHandle) -> Result<PrincipleCounts, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare("select state, origin, count(*) from principles group by state, origin")
        .map_err(|err| format!("准备 count 失败：{err}"))?;
    let mut counts = PrincipleCounts {
        proposed: 0,
        active: 0,
        dormant: 0,
        retired: 0,
        user_stated: 0,
        agent_inferred: 0,
    };
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u32>(2)?,
            ))
        })
        .map_err(|err| format!("query count 失败：{err}"))?;
    for r in rows {
        let (state, origin, n) = r.map_err(|err| format!("collect count 失败：{err}"))?;
        match state.as_str() {
            "proposed" => counts.proposed += n,
            "active" => counts.active += n,
            "dormant" => counts.dormant += n,
            "retired" => counts.retired += n,
            _ => {}
        }
        match origin.as_str() {
            "user_stated" => counts.user_stated += n,
            "agent_inferred" => counts.agent_inferred += n,
            _ => {}
        }
    }
    Ok(counts)
}

// ====== 内部 ============================================================

fn list_internal(
    app: &AppHandle,
    state_filter: Option<PrincipleState>,
    origin_filter: Option<PrincipleOrigin>,
    limit: i64,
) -> Result<Vec<Principle>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let (sql, has_state, has_origin) = match (state_filter, origin_filter) {
        (Some(_), Some(_)) => (
            "select id, body, category, origin, state, regime_tags, hit_count,
                    last_applied_at, created_at
             from principles where state = ?1 and origin = ?2
             order by hit_count desc, created_at desc limit ?3",
            true,
            true,
        ),
        (Some(_), None) => (
            "select id, body, category, origin, state, regime_tags, hit_count,
                    last_applied_at, created_at
             from principles where state = ?1
             order by hit_count desc, created_at desc limit ?2",
            true,
            false,
        ),
        (None, Some(_)) => (
            "select id, body, category, origin, state, regime_tags, hit_count,
                    last_applied_at, created_at
             from principles where origin = ?1
             order by hit_count desc, created_at desc limit ?2",
            false,
            true,
        ),
        (None, None) => (
            "select id, body, category, origin, state, regime_tags, hit_count,
                    last_applied_at, created_at
             from principles
             order by hit_count desc, created_at desc limit ?1",
            false,
            false,
        ),
    };
    let mut stmt = conn
        .prepare(sql)
        .map_err(|err| format!("准备 list_internal 失败：{err}"))?;
    let map_row = |row: &rusqlite::Row<'_>| {
        Ok((
            row.get::<_, String>(0)?,
            PrincipleRow {
                body: row.get(1)?,
                category: row.get(2)?,
                origin: row.get(3)?,
                state: row.get(4)?,
                regime_tags: row.get(5)?,
                hit_count: row.get(6)?,
                last_applied_at: row.get(7)?,
                created_at: row.get(8)?,
            },
        ))
    };
    let rows: Vec<(String, PrincipleRow)> = match (has_state, has_origin) {
        (true, true) => {
            let st = state_filter.unwrap();
            let or = origin_filter.unwrap();
            stmt.query_map(params![st.as_str(), or.as_str(), limit], map_row)
                .map_err(|err| format!("query list 失败：{err}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|err| format!("collect list 失败：{err}"))?
        }
        (true, false) => {
            let st = state_filter.unwrap();
            stmt.query_map(params![st.as_str(), limit], map_row)
                .map_err(|err| format!("query list 失败：{err}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|err| format!("collect list 失败：{err}"))?
        }
        (false, true) => {
            let or = origin_filter.unwrap();
            stmt.query_map(params![or.as_str(), limit], map_row)
                .map_err(|err| format!("query list 失败：{err}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|err| format!("collect list 失败：{err}"))?
        }
        (false, false) => stmt
            .query_map(params![limit], map_row)
            .map_err(|err| format!("query list 失败：{err}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| format!("collect list 失败：{err}"))?,
    };
    let mut out = Vec::with_capacity(rows.len());
    for (id_str, row) in rows {
        out.push(row.into_domain(PrincipleId::from_string(id_str))?);
    }
    Ok(out)
}

struct PrincipleRow {
    body: String,
    category: String,
    origin: String,
    state: String,
    regime_tags: Option<String>,
    hit_count: u32,
    last_applied_at: Option<String>,
    created_at: String,
}

impl PrincipleRow {
    fn into_domain(self, id: PrincipleId) -> Result<Principle, String> {
        let category = PrincipleCategory::parse(&self.category)
            .ok_or_else(|| format!("未知 category: {}", self.category))?;
        let origin = PrincipleOrigin::parse(&self.origin)
            .ok_or_else(|| format!("未知 origin: {}", self.origin))?;
        let state = PrincipleState::parse(&self.state)
            .ok_or_else(|| format!("未知 state: {}", self.state))?;
        let regime_tags: Vec<Regime> = match self.regime_tags {
            Some(json) => {
                let tags: Vec<String> = serde_json::from_str(&json)
                    .map_err(|err| format!("反序列化 regime_tags 失败：{err}"))?;
                tags.iter().filter_map(|t| Regime::parse(t)).collect()
            }
            None => Vec::new(),
        };
        Ok(Principle {
            id,
            body: self.body,
            category,
            origin,
            state,
            regime_tags,
            hit_count: self.hit_count,
            last_applied_at: match self.last_applied_at {
                Some(s) => Some(parse_occurred(&s)?),
                None => None,
            },
            created_at: parse_occurred(&self.created_at)?,
        })
    }
}

fn parse_occurred(s: &str) -> Result<OccurredAt, String> {
    let dt = chrono::DateTime::parse_from_rfc3339(s)
        .map_err(|err| format!("解析 RFC3339 失败 ({s}): {err}"))?;
    Ok(OccurredAt::new(dt.timestamp_millis()))
}
