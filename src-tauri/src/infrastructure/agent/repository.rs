//! Agent 子域 DB 访问——chat_messages + agent_episodes + agent_episode_turns 三张表。
//!
//! 表设计（v2 重构后）：
//! - `chat_messages`：对话流（id PK / role / kind / content_md / content_json / source_*）
//! - `agent_episodes`：每次 run 的统计 + trigger_kind / thesis_ids / outcome_summary
//!   （run_id PK / trigger_kind / model / turns / tokens / stop_reason）
//! - `agent_episode_turns`：每个 turn 的细粒度统计（(run_id, turn) PK）
//!
//! 写路径：pipeline::chat / pipeline::agent::observer 调 append + finalize。
//! 读路径：Tauri IPC list/search 给前端 chat UI 用；read_all 给 agent 历史上下文用。

use crate::infrastructure::db::{json_string, migrate, now, open_database, required_json_string};
use rusqlite::{params, Connection};
use serde_json::Value;
use tauri::{AppHandle, Emitter};

pub fn append_chat_message(app: AppHandle, message: Value) -> Result<Value, String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let id = required_json_string(&message, "/id", "对话消息缺少 id")?;
    let created_at =
        json_string(&message, "/createdAt").unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    let role = required_json_string(&message, "/role", "对话消息缺少 role")?;
    let kind = required_json_string(&message, "/kind", "对话消息缺少 kind")?;
    let content_md = required_json_string(&message, "/contentMd", "对话消息缺少 contentMd")?;
    let content_json = message
        .pointer("/contentJson")
        .filter(|v| !v.is_null())
        .map(|v| v.to_string());
    let source_task_id = json_string(&message, "/sourceTaskId");
    let source_record_id = json_string(&message, "/sourceRecordId");
    let source_news_ids = message
        .pointer("/sourceNewsIds")
        .filter(|v| !v.is_null())
        .map(|v| v.to_string());

    connection
        .execute(
            "insert into chat_messages
                (id, created_at, role, kind, content_md, content_json,
                 source_task_id, source_news_ids, source_record_id)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                created_at,
                role,
                kind,
                content_md,
                content_json,
                source_task_id,
                source_news_ids,
                source_record_id,
            ],
        )
        .map_err(|err| format!("写入对话消息失败：{err}"))?;
    let stored = read_chat_message(&connection, &id)?;
    let _ = app.emit("chat-message-appended", stored.clone());
    Ok(stored)
}

/// 前端分页用——UI 滚动加载 chat 历史。clamp 到 1..=200 保护渲染层不被一次性数千行打爆。
///
/// **不要给 agent pipeline 用**——agent 读历史应该走 `read_all_chat_messages` 拿全量，
/// 由 compact tier（MicroClear → Summarize → Drop）决定语义截断。
pub fn list_chat_messages(
    app: AppHandle,
    before: Option<String>,
    limit: Option<i64>,
) -> Result<Vec<Value>, String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let cap = limit.unwrap_or(50).clamp(1, 200);
    let mut statement = connection
        .prepare(
            "select id, created_at, role, kind, content_md, content_json,
                    source_task_id, source_news_ids, source_record_id
             from chat_messages
             where (?1 is null or created_at < ?1)
             order by created_at desc
             limit ?2",
        )
        .map_err(|err| format!("读取对话消息失败：{err}"))?;
    let rows = statement
        .query_map(params![before, cap], row_to_chat_message)
        .map_err(|err| format!("读取对话消息失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("读取对话消息失败：{err}"))?;
    Ok(rows)
}

/// Agent pipeline 专用——读取 chat_messages 全量历史，**不做条数截断**。
///
/// 与 `list_chat_messages` 区别：
/// - 不暴露为 Tauri command（仅 Rust 内部调用）；
/// - 没有 UI 友好的 200 行钳位——agent 拿到的就该是 DB 里 `before` 之前的所有行；
/// - 语义截断由上层 `read_recent_chat_thread` + compact tier 处理。
///
/// 主流共识（Cursor / Cline / Roo / Aider / OpenAI Agents SDK / LangGraph /
/// Anthropic Cookbook）：DB 读取层不截语义，先读全，让 compaction 层按 token 决定。
///
/// `before` 用于游标分页（一般不传，pipeline 拿最新到现在）。
pub fn read_all_chat_messages(app: &AppHandle, before: Option<&str>) -> Result<Vec<Value>, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    let mut statement = connection
        .prepare(
            "select id, created_at, role, kind, content_md, content_json,
                    source_task_id, source_news_ids, source_record_id
             from chat_messages
             where (?1 is null or created_at < ?1)
             order by created_at desc",
        )
        .map_err(|err| format!("读取对话消息失败：{err}"))?;
    let rows = statement
        .query_map(params![before], row_to_chat_message)
        .map_err(|err| format!("读取对话消息失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("读取对话消息失败：{err}"))?;
    Ok(rows)
}

pub fn search_chat_messages(
    app: AppHandle,
    query: String,
    limit: Option<i64>,
) -> Result<Vec<Value>, String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let cap = limit.unwrap_or(50).clamp(1, 200);
    let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
    let mut statement = connection
        .prepare(
            "select id, created_at, role, kind, content_md, content_json,
                    source_task_id, source_news_ids, source_record_id
             from chat_messages
             where content_md like ?1 escape '\\'
             order by created_at desc
             limit ?2",
        )
        .map_err(|err| format!("搜索对话消息失败：{err}"))?;
    let rows = statement
        .query_map(params![pattern, cap], row_to_chat_message)
        .map_err(|err| format!("搜索对话消息失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("搜索对话消息失败：{err}"))?;
    Ok(rows)
}

/// agent_episodes 表的写入入口——在 run 启动时插一条 started_at；run 结束时
/// update token / turns / stop_reason / ended_at。observer.rs 调这两个。
///
/// 参数 `trigger_kind` 取代旧 `pipeline`：取值之一 `scheduled / user_message /
/// user_instruction / reflection / chat`。Phase 1 用户驱动统一传 'chat'，
/// 后续 reflection pipeline 传 'reflection'，scheduler 触发的传 'scheduled'。
#[allow(clippy::too_many_arguments)]
pub fn insert_agent_episode_start(
    app: &AppHandle,
    run_id: &str,
    trigger_kind: &str,
    trigger_ref: Option<&str>,
    provider: &str,
    model: &str,
    started_at: &str,
    trigger_message_id: Option<&str>,
    parent_episode_id: Option<&str>,
) -> Result<(), String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    connection
        .execute(
            "insert into agent_episodes
                (run_id, trigger_kind, trigger_ref, provider, model, started_at,
                 trigger_message_id, parent_episode_id)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                run_id,
                trigger_kind,
                trigger_ref,
                provider,
                model,
                started_at,
                trigger_message_id,
                parent_episode_id
            ],
        )
        .map_err(|err| format!("写 agent_episodes 失败：{err}"))?;
    Ok(())
}

/// 每个 turn 收尾时落一行 agent_episode_turns。
/// 投资学习的关键审计点——给错答案时能 grep 出第几 turn 出岔子。
#[allow(clippy::too_many_arguments)]
pub fn insert_agent_episode_turn(
    app: &AppHandle,
    run_id: &str,
    turn: u32,
    started_at: &str,
    ended_at: &str,
    stop_reason: Option<&str>,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    local_tool_calls: u32,
    server_tool_calls: u32,
    error: Option<&str>,
) -> Result<(), String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    connection
        .execute(
            "insert or replace into agent_episode_turns
                (run_id, turn, started_at, ended_at, stop_reason,
                 input_tokens, output_tokens, cache_read_tokens,
                 local_tool_calls, server_tool_calls, error)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                run_id,
                turn,
                started_at,
                ended_at,
                stop_reason,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                local_tool_calls,
                server_tool_calls,
                error,
            ],
        )
        .map_err(|err| format!("写 agent_episode_turns 失败：{err}"))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn finalize_agent_episode(
    app: &AppHandle,
    run_id: &str,
    ended_at: &str,
    turns: u32,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    cache_write_tokens: u32,
    local_tool_calls: u32,
    server_tool_calls: u32,
    stop_reason: Option<&str>,
    error: Option<&str>,
    thesis_ids: Option<&str>,
    outcome_summary: Option<&str>,
) -> Result<(), String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    connection
        .execute(
            "update agent_episodes set
                ended_at = ?2,
                turns = ?3,
                input_tokens = ?4,
                output_tokens = ?5,
                cache_read_tokens = ?6,
                cache_write_tokens = ?7,
                local_tool_calls = ?8,
                server_tool_calls = ?9,
                stop_reason = ?10,
                error = ?11,
                thesis_ids = ?12,
                outcome_summary = ?13
             where run_id = ?1",
            params![
                run_id,
                ended_at,
                turns,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                local_tool_calls,
                server_tool_calls,
                stop_reason,
                error,
                thesis_ids,
                outcome_summary,
            ],
        )
        .map_err(|err| format!("更新 agent_episodes 失败：{err}"))?;
    Ok(())
}

fn read_chat_message(connection: &Connection, id: &str) -> Result<Value, String> {
    connection
        .query_row(
            "select id, created_at, role, kind, content_md, content_json,
                    source_task_id, source_news_ids, source_record_id
             from chat_messages where id = ?1",
            params![id],
            row_to_chat_message,
        )
        .map_err(|err| format!("读取对话消息失败：{err}"))
}

fn row_to_chat_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let content_json_raw: Option<String> = row.get(5)?;
    let source_news_raw: Option<String> = row.get(7)?;
    Ok(serde_json::json!({
        "id": row.get::<_, String>(0)?,
        "createdAt": row.get::<_, String>(1)?,
        "role": row.get::<_, String>(2)?,
        "kind": row.get::<_, String>(3)?,
        "contentMd": row.get::<_, String>(4)?,
        "contentJson": content_json_raw
            .as_deref()
            .map(|text| serde_json::from_str::<Value>(text).unwrap_or(Value::Null))
            .unwrap_or(Value::Null),
        "sourceTaskId": row.get::<_, Option<String>>(6)?,
        "sourceNewsIds": source_news_raw
            .as_deref()
            .map(|text| serde_json::from_str::<Value>(text).unwrap_or(Value::Null))
            .unwrap_or(Value::Null),
        "sourceRecordId": row.get::<_, Option<String>>(8)?,
    }))
}
