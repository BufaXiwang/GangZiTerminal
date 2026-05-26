//! 调度器心跳 + 健康度——跨 BC 的 cross-cutting 观测设施。
//!
//! 每个后台 loop (news / market / account / kline_warm / reflection / scan) 在每次
//! tick 完成时调 `record_ok` / `record_err`，写到 scheduler_heartbeat 表。
//!
//! 前端通过 `list_heartbeats` 查最新一次心跳——能直接看出"某个 loop 卡了多久"。
//!
//! 这是观测 *第 4 条硬缺口* 的入口：以前后台 loop 静默失败用户无感，现在心跳表把状态搬到前台。

use crate::infrastructure::db::{migrate, now, open_database};
use rusqlite::params;
use serde::Serialize;
use tauri::AppHandle;

/// 已知的 loop 名（拼写一致性靠这里——所有 caller 都用这些常量）
pub const LOOP_NEWS: &str = "news";
pub const LOOP_MARKET_QUOTE: &str = "market_quote";
pub const LOOP_MARKET_UNIVERSE: &str = "market_universe";
pub const LOOP_ACCOUNT: &str = "account_close";
pub const LOOP_KLINE_WARM: &str = "kline_warm";
// LOOP_NEWS_RETENTION 已移除（spec news-module.md §65：News 不主动 retention）。
// Agent Runtime loops（spec §10 可观测覆盖范围）
pub const LOOP_AGENT_NEWS_BATCH: &str = "agent_news_batch";
/// spec §10 可观测 loop——account_trigger_routing 当前在 router 内联，未独立写 heartbeat。
#[allow(dead_code)]
pub const LOOP_AGENT_ACCOUNT_TRIGGER_ROUTING: &str = "agent_account_trigger_routing";
/// spec §10 可观测 loop——scheduled_review tick 启用后写 heartbeat（当前未启用）。
#[allow(dead_code)]
pub const LOOP_AGENT_SCHEDULED_REVIEW: &str = "agent_scheduled_review";
pub const LOOP_ACCOUNT_TRIGGER_EVAL: &str = "account_trigger_eval";

/// SchedulerHeartbeat 行——字段名严格对齐 spec `agent-runtime-module.md §10`：
/// `loopName / lastOkAt / lastErrorAt / lastError / consecutiveFailures`。
/// DB 列名沿用旧 schema 不动；只改对外 JSON 字段名。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatRow {
    pub loop_name: String,
    pub last_ok_at: Option<String>,
    #[serde(rename = "lastErrorAt")]
    pub last_err_at: Option<String>,
    #[serde(rename = "lastError")]
    pub last_err_msg: Option<String>,
    #[serde(rename = "consecutiveFailures")]
    pub consecutive_err: u32,
    pub updated_at: String,
}

/// 记录一次成功的 tick——清空 consecutive_err 计数。
pub fn record_ok(app: &AppHandle, loop_name: &str) {
    if let Err(e) = upsert(app, loop_name, true, None) {
        tracing::warn!(loop_name, error = %e, "heartbeat record_ok 写库失败（忽略）");
    }
}

/// 记录一次失败的 tick——累加 consecutive_err，写错误信息。
pub fn record_err(app: &AppHandle, loop_name: &str, msg: &str) {
    if let Err(e) = upsert(app, loop_name, false, Some(msg)) {
        tracing::warn!(loop_name, error = %e, "heartbeat record_err 写库失败（忽略）");
    }
}

pub fn list_heartbeats(app: &AppHandle) -> Result<Vec<HeartbeatRow>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare(
            "select loop_name, last_ok_at, last_err_at, last_err_msg, consecutive_err, updated_at
             from scheduler_heartbeat
             order by loop_name",
        )
        .map_err(|e| format!("准备 heartbeat 查询失败：{e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(HeartbeatRow {
                loop_name: r.get(0)?,
                last_ok_at: r.get(1)?,
                last_err_at: r.get(2)?,
                last_err_msg: r.get(3)?,
                consecutive_err: r.get::<_, i64>(4).unwrap_or(0) as u32,
                updated_at: r.get(5)?,
            })
        })
        .map_err(|e| format!("执行 heartbeat 查询失败：{e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("收集 heartbeat 行失败：{e}"))
}

fn upsert(app: &AppHandle, loop_name: &str, ok: bool, err_msg: Option<&str>) -> Result<(), String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let ts = now();
    if ok {
        conn.execute(
            "insert into scheduler_heartbeat (loop_name, last_ok_at, last_err_at, last_err_msg, consecutive_err, updated_at)
             values (?1, ?2, null, null, 0, ?2)
             on conflict(loop_name) do update set
                last_ok_at = excluded.last_ok_at,
                consecutive_err = 0,
                updated_at = excluded.updated_at",
            params![loop_name, ts],
        )
        .map_err(|e| format!("upsert heartbeat ok 失败：{e}"))?;
    } else {
        conn.execute(
            "insert into scheduler_heartbeat (loop_name, last_ok_at, last_err_at, last_err_msg, consecutive_err, updated_at)
             values (?1, null, ?2, ?3, 1, ?2)
             on conflict(loop_name) do update set
                last_err_at = excluded.last_err_at,
                last_err_msg = excluded.last_err_msg,
                consecutive_err = scheduler_heartbeat.consecutive_err + 1,
                updated_at = excluded.updated_at",
            params![loop_name, ts, err_msg],
        )
        .map_err(|e| format!("upsert heartbeat err 失败：{e}"))?;
    }
    Ok(())
}
