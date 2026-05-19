//! Strategy 持久化——config_json 整体存读，hit/miss/applied 计数原子更新。

use crate::domain::agent::strategy::{
    SignalCondition, Strategy, StrategyId, TargetRule, TriggerLogic,
};
use crate::domain::shared::OccurredAt;
use crate::infrastructure::db::{migrate, open_database};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

pub const EVENT_STRATEGIES_CHANGED: &str = "strategies-changed";

fn emit_changed(app: &AppHandle) {
    let _ = app.emit(EVENT_STRATEGIES_CHANGED, serde_json::json!({}));
}

/// 落库形态：除统计列外，其他字段全压进 config_json。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StrategyConfig {
    pub trigger_when: Vec<SignalCondition>,
    pub trigger_logic: TriggerLogic,
    pub target: TargetRule,
    pub conviction_rule: crate::domain::agent::strategy::ConvictionRule,
}

pub fn create(app: &AppHandle, s: &Strategy) -> Result<(), String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let cfg = StrategyConfig {
        trigger_when: s.trigger_when.clone(),
        trigger_logic: s.trigger_logic,
        target: s.target,
        conviction_rule: s.conviction_rule.clone(),
    };
    let cfg_json = serde_json::to_string(&cfg)
        .map_err(|err| format!("序列化 strategy config 失败：{err}"))?;
    conn.execute(
        "insert into strategies
            (id, name, description, config_json, enabled, applied_count, hit_count, miss_count, created_at, updated_at)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            s.id.as_str(),
            s.name,
            s.description,
            cfg_json,
            if s.enabled { 1i64 } else { 0i64 },
            s.applied_count as i64,
            s.hit_count as i64,
            s.miss_count as i64,
            s.created_at.to_rfc3339(),
            s.updated_at.to_rfc3339(),
        ],
    )
    .map_err(|err| format!("插入 strategy 失败：{err}"))?;
    emit_changed(app);
    Ok(())
}

pub fn set_enabled(
    app: &AppHandle,
    id: &StrategyId,
    enabled: bool,
    now: OccurredAt,
) -> Result<(), String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    conn.execute(
        "update strategies set enabled = ?2, updated_at = ?3 where id = ?1",
        params![id.as_str(), if enabled { 1i64 } else { 0i64 }, now.to_rfc3339()],
    )
    .map_err(|err| format!("更新 strategy enabled 失败：{err}"))?;
    emit_changed(app);
    Ok(())
}

pub fn increment_applied(app: &AppHandle, id: &StrategyId) -> Result<(), String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    conn.execute(
        "update strategies set applied_count = applied_count + 1 where id = ?1",
        params![id.as_str()],
    )
    .map_err(|err| format!("递增 applied_count 失败：{err}"))?;
    emit_changed(app);
    Ok(())
}

pub fn record_outcome(
    app: &AppHandle,
    id: &StrategyId,
    outcome: bool,
) -> Result<(), String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let column = if outcome { "hit_count" } else { "miss_count" };
    let sql = format!(
        "update strategies set {col} = {col} + 1 where id = ?1",
        col = column
    );
    conn.execute(&sql, params![id.as_str()])
        .map_err(|err| format!("记录 strategy outcome 失败：{err}"))?;
    emit_changed(app);
    Ok(())
}

pub fn get(app: &AppHandle, id: &StrategyId) -> Result<Option<Strategy>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let row = conn
        .query_row(
            "select name, description, config_json, enabled, applied_count, hit_count, miss_count,
                    created_at, updated_at
             from strategies where id = ?1",
            params![id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, u32>(4)?,
                    row.get::<_, u32>(5)?,
                    row.get::<_, u32>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )
        .optional()
        .map_err(|err| format!("读取 strategy 失败：{err}"))?;
    let Some((name, desc, cfg_json, enabled, applied, hit, miss, created_at, updated_at)) = row
    else {
        return Ok(None);
    };
    let cfg: StrategyConfig = serde_json::from_str(&cfg_json)
        .map_err(|err| format!("反序列化 strategy config 失败：{err}"))?;
    Ok(Some(Strategy {
        id: id.clone(),
        name,
        description: desc.unwrap_or_default(),
        trigger_when: cfg.trigger_when,
        trigger_logic: cfg.trigger_logic,
        target: cfg.target,
        conviction_rule: cfg.conviction_rule,
        enabled: enabled != 0,
        applied_count: applied,
        hit_count: hit,
        miss_count: miss,
        created_at: parse_occurred(&created_at)?,
        updated_at: parse_occurred(&updated_at)?,
    }))
}

pub fn list_enabled(app: &AppHandle) -> Result<Vec<Strategy>, String> {
    list_internal(app, true)
}

pub fn list_all(app: &AppHandle) -> Result<Vec<Strategy>, String> {
    list_internal(app, false)
}

fn list_internal(app: &AppHandle, enabled_only: bool) -> Result<Vec<Strategy>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let sql = if enabled_only {
        "select id, name, description, config_json, enabled, applied_count, hit_count, miss_count,
                created_at, updated_at
         from strategies where enabled = 1 order by created_at"
    } else {
        "select id, name, description, config_json, enabled, applied_count, hit_count, miss_count,
                created_at, updated_at
         from strategies order by enabled desc, created_at"
    };
    let mut stmt = conn
        .prepare(sql)
        .map_err(|err| format!("准备 list strategies 失败：{err}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, u32>(5)?,
                row.get::<_, u32>(6)?,
                row.get::<_, u32>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
            ))
        })
        .map_err(|err| format!("query strategies 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect strategies 失败：{err}"))?;
    let mut out = Vec::with_capacity(rows.len());
    for (id, name, desc, cfg_json, enabled, applied, hit, miss, created_at, updated_at) in rows {
        let cfg: StrategyConfig = serde_json::from_str(&cfg_json)
            .map_err(|err| format!("反序列化 strategy config 失败：{err}"))?;
        out.push(Strategy {
            id: StrategyId::from_string(id),
            name,
            description: desc.unwrap_or_default(),
            trigger_when: cfg.trigger_when,
            trigger_logic: cfg.trigger_logic,
            target: cfg.target,
            conviction_rule: cfg.conviction_rule,
            enabled: enabled != 0,
            applied_count: applied,
            hit_count: hit,
            miss_count: miss,
            created_at: parse_occurred(&created_at)?,
            updated_at: parse_occurred(&updated_at)?,
        });
    }
    Ok(out)
}

fn parse_occurred(s: &str) -> Result<OccurredAt, String> {
    let dt = chrono::DateTime::parse_from_rfc3339(s)
        .map_err(|err| format!("解析 RFC3339 失败 ({s}): {err}"))?;
    Ok(OccurredAt::new(dt.timestamp_millis()))
}
