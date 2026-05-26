//! `agent_tool_calls` audit persistence —— spec `agent-infra-module.md §2`.
//!
//! 每次 local + server-side tool 调用都必须落一行。当前阶段写入摘要（input/
//! output summary）；spec 要求的 `*_payload_ref` 用于恢复结构化副作用的工具
//! （如 `operate_account`），后续把 payload 落到独立详情表 / object store
//! 后回填 ref。

use crate::infrastructure::db::helpers::now;
use crate::infrastructure::db::{migrate, open_database};
use rusqlite::{params, Connection};
use tauri::AppHandle;

fn conn(app: &AppHandle) -> Result<Connection, String> {
    let c = open_database(app)?;
    migrate(&c)?;
    Ok(c)
}

#[derive(Debug, Clone)]
pub struct ToolCallRow<'a> {
    pub tool_call_id: &'a str,
    pub run_id: &'a str,
    pub name: &'a str,
    /// "local_tool" | "server_side_tool"
    pub source: &'a str,
    pub input_summary_json: Option<&'a str>,
    pub output_summary_json: Option<&'a str>,
    pub input_payload_ref: Option<&'a str>,
    pub output_payload_ref: Option<&'a str>,
    pub is_error: bool,
    pub error_code: Option<&'a str>,
    pub started_at: &'a str,
    pub ended_at: Option<&'a str>,
    pub duration_ms: Option<i64>,
}

/// 查 run 内某 tool_call_id 是否存在（spec evidence hydrate `kind=tool_call,
/// source=tool_result` 校验用）。
pub fn exists_in_run(app: &AppHandle, run_id: &str, tool_call_id: &str) -> Result<bool, String> {
    let c = conn(app)?;
    let n: i64 = c
        .query_row(
            "select count(*) from agent_tool_calls where run_id = ?1 and tool_call_id = ?2",
            params![run_id, tool_call_id],
            |r| r.get(0),
        )
        .map_err(|e| format!("query tool_call 失败：{e}"))?;
    Ok(n > 0)
}

/// 列出 run 内所有 tool call 摘要（hydrate evidence 时如果想 fallback 找
/// 同 run 引用的 tool_call_id 用）。
pub fn list_for_run(app: &AppHandle, run_id: &str) -> Result<Vec<(String, String, bool)>, String> {
    let c = conn(app)?;
    let mut stmt = c
        .prepare(
            "select tool_call_id, name, is_error from agent_tool_calls
             where run_id = ?1 order by started_at asc",
        )
        .map_err(|e| format!("prepare 失败：{e}"))?;
    let mut rows = stmt
        .query(params![run_id])
        .map_err(|e| format!("query 失败：{e}"))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(|e| format!("next 失败：{e}"))? {
        out.push((
            r.get::<_, String>(0).map_err(|e| e.to_string())?,
            r.get::<_, String>(1).map_err(|e| e.to_string())?,
            r.get::<_, i64>(2).map_err(|e| e.to_string())? != 0,
        ));
    }
    Ok(out)
}

pub fn insert(app: &AppHandle, row: ToolCallRow<'_>) -> Result<(), String> {
    let c = conn(app)?;
    c.execute(
        "insert or replace into agent_tool_calls(
            tool_call_id, run_id, name, source,
            input_summary_json, output_summary_json,
            input_payload_ref, output_payload_ref,
            is_error, error_code, started_at, ended_at, duration_ms
         ) values (?1,?2,?3,?4, ?5,?6, ?7,?8, ?9,?10, ?11,?12,?13)",
        params![
            row.tool_call_id,
            row.run_id,
            row.name,
            row.source,
            row.input_summary_json,
            row.output_summary_json,
            row.input_payload_ref,
            row.output_payload_ref,
            if row.is_error { 1i64 } else { 0i64 },
            row.error_code,
            row.started_at,
            row.ended_at,
            row.duration_ms,
        ],
    )
    .map_err(|e| format!("写 agent_tool_call 失败：{e}"))?;
    let _ = now();
    Ok(())
}

/// spec §2「需要恢复副作用或审计精确结果的 local tool 必须持久化结构化 input /
/// output payload」。operate_account 等写工具调完后写 payload；recover_submitted
/// 优先读它做 TradeIntent 状态恢复。
pub fn set_output_payload(
    app: &AppHandle,
    tool_call_id: &str,
    payload_json: &str,
) -> Result<(), String> {
    let c = conn(app)?;
    c.execute(
        "update agent_tool_calls set output_payload_json = ?2 where tool_call_id = ?1",
        params![tool_call_id, payload_json],
    )
    .map_err(|e| format!("set output_payload_json 失败：{e}"))?;
    Ok(())
}

pub fn read_output_payload(
    app: &AppHandle,
    tool_call_id: &str,
) -> Result<Option<String>, String> {
    let c = conn(app)?;
    Ok(c.query_row(
        "select output_payload_json from agent_tool_calls where tool_call_id = ?1",
        params![tool_call_id],
        |r| r.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten())
}
