//! Agent message / ToolCall 持久化。
//!
//! Spec: docs/design/agent-infra-module.md §2 不变量
//! - 所有 tool 调用必须先通过 `ToolRegistry` 校验。
//! - 所有 tool 调用都必须记录 `ToolCall`。

use crate::domain::agent::{AgentMessage, MessageKind, ToolCall};
use crate::domain::shared::ErrorCode;
use crate::infrastructure::db::AppDb;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// 对话列表条目（前端 sidebar 用）。
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSummary {
    pub conversation_id: String,
    pub message_count: u32,
    pub first_at: String,
    pub last_at: String,
    pub preview: String,
}

/// Agent BC 持久化 repo。
#[derive(Clone)]
pub struct AgentMessagesRepo {
    db: AppDb,
}

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("invalid row: {0}")]
    InvalidRow(&'static str),
}

impl AgentMessagesRepo {
    pub fn new(db: AppDb) -> Self {
        Self { db }
    }

    /// 持久化一条消息。
    ///
    /// Spec: agent-infra-module.md §2 `AgentMessage`
    pub fn upsert_message(&self, msg: &AgentMessage) -> Result<(), RepoError> {
        let blocks_json = serde_json::to_string(&msg.blocks)?;
        let role = serde_json::to_value(msg.role)?
            .as_str()
            .ok_or(RepoError::InvalidRow("role"))?
            .to_string();
        let kind = match msg.kind {
            Some(k) => Some(
                serde_json::to_value(k)?
                    .as_str()
                    .ok_or(RepoError::InvalidRow("kind"))?
                    .to_string(),
            ),
            None => None,
        };
        self.db.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO agent_messages
                  (message_id, run_id, conversation_id, seq, kind, role, blocks_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    msg.message_id,
                    msg.run_id,
                    msg.conversation_id,
                    msg.seq,
                    kind,
                    role,
                    blocks_json,
                    msg.created_at.to_rfc3339()
                ],
            )?;
            Ok::<_, RepoError>(())
        })
    }

    /// 把一条已持久化消息标记为 durable（spec §4：trading_write 轮的 tool_result 永不压缩 / 替 stub /
    /// 折进摘要——标记落库使该保护**跨 run 续接**仍生效）。`upsert_message` 不触碰该列
    ///（INSERT OR REPLACE 后由 loop 在需要时重新打标）。
    pub fn mark_durable(&self, message_id: &str) -> Result<(), RepoError> {
        self.db.with(|c| {
            c.execute(
                "UPDATE agent_messages SET durable = 1 WHERE message_id = ?1",
                params![message_id],
            )?;
            Ok::<_, RepoError>(())
        })
    }

    /// 读某会话全部 durable 消息 id（loop 续接时重建 durable_message_ids，spec §4）。
    pub fn load_durable_ids(&self, conversation_id: &str) -> Result<Vec<String>, RepoError> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT message_id FROM agent_messages
                 WHERE conversation_id = ?1 AND durable = 1",
            )?;
            let rows = stmt
                .query_map(params![conversation_id], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// 给定 conversation 的下一个 seq（max(seq)+1，无消息则 0）。
    ///
    /// Spec: agent-infra-module.md §4 多轮会话持久化（会话内单调序号）。
    pub fn next_seq(&self, conversation_id: &str) -> Result<i64, RepoError> {
        self.db.with(|c| {
            let max: Option<i64> = c.query_row(
                "SELECT MAX(seq) FROM agent_messages WHERE conversation_id = ?1",
                params![conversation_id],
                |r| r.get(0),
            )?;
            Ok::<_, RepoError>(max.map(|m| m + 1).unwrap_or(0))
        })
    }

    /// 全量加载会话（按 seq 排序）—— 审计真源。
    ///
    /// Spec: agent-infra-module.md §5 `load_conversation`。seq 为 NULL 的行排到末尾（按 created_at）。
    pub fn load_conversation(&self, conversation_id: &str) -> Result<Vec<AgentMessage>, RepoError> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT message_id, run_id, conversation_id, seq, kind, role, blocks_json, created_at
                 FROM agent_messages WHERE conversation_id = ?1
                 ORDER BY seq IS NULL, seq ASC, created_at ASC, message_id ASC",
            )?;
            let rows = stmt
                .query_map(params![conversation_id], row_to_message)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// 加载会话的压缩视图（续接用，不返回全量）。
    ///
    /// Spec: agent-infra-module.md §5 `load_conversation_view` / §4 续接：
    /// 最近一个 `kind=summary` 检查点 + 其后（seq 更大）的所有消息；无 summary → 返回全量。
    ///
    /// **有界性来自 `apply_summary`（loop_executor）的「边界 seq」选择**：滚动摘要继承「被压缩区间
    /// 最大 seq」，故在审计序里紧贴保留尾窗之前，「其后」= 仅摘要 + 最近若干轮 → 有界。若改回让摘要
    /// 继承最旧 seq，此处会退化为「摘要 + 其后全部历史」随轮数线性膨胀（回归）。摘要 seq 与边界原始
    /// 消息相同时，平局由 `created_at` / `message_id` 打破（摘要晚于原始消息，排其后）。
    pub fn load_conversation_view(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<AgentMessage>, RepoError> {
        let all = self.load_conversation(conversation_id)?;
        // 找最后一个 summary 检查点的位置（按已排序顺序）。
        let last_summary_idx = all
            .iter()
            .rposition(|m| m.kind == Some(MessageKind::Summary));
        match last_summary_idx {
            Some(i) => {
                // Spec §4：durable（trading_write）消息**永远 inline 保留**——它们不折进摘要，
                // seq 落在 summary 之前，若只取 `summary..` 会被挤出视野（跨 run 即丢，违反不变量）。
                // 视图 = summary 前的 durable 行（按原序）+ summary + 其后全部。
                let durable: std::collections::HashSet<String> =
                    self.load_durable_ids(conversation_id)?.into_iter().collect();
                let mut view: Vec<AgentMessage> = all[..i]
                    .iter()
                    .filter(|m| durable.contains(&m.message_id))
                    .cloned()
                    .collect();
                view.extend_from_slice(&all[i..]);
                Ok(view)
            }
            None => Ok(all),
        }
    }

    /// 列出所有用户会话（按最近活跃排序），用于前端 sidebar 展示。
    ///
    /// 过滤掉 `fork:` 前缀的子 agent 会话（spec §3.5/§3.6）。
    pub fn list_conversations(&self) -> Result<Vec<ConversationSummary>, RepoError> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT conversation_id, COUNT(*) as cnt,
                        MIN(created_at) as first_at,
                        MAX(created_at) as last_at
                 FROM agent_messages
                 WHERE conversation_id IS NOT NULL
                   AND role IN ('user', 'assistant')
                   AND conversation_id NOT LIKE 'fork:%'
                 GROUP BY conversation_id
                 ORDER BY last_at DESC",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(ConversationSummary {
                        conversation_id: row.get(0)?,
                        message_count: row.get(1)?,
                        first_at: row.get(2)?,
                        last_at: row.get(3)?,
                        preview: String::new(),
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;

            let mut result = rows;
            for conv in &mut result {
                if let Ok(Some(preview)) = c
                    .query_row(
                        "SELECT blocks_json FROM agent_messages
                         WHERE conversation_id = ?1 AND role = 'user'
                         ORDER BY created_at ASC LIMIT 1",
                        [&conv.conversation_id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()
                    .map(|o| o.flatten())
                {
                    if let Ok(blocks) = serde_json::from_str::<Vec<serde_json::Value>>(&preview) {
                        conv.preview = blocks
                            .iter()
                            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(50)
                            .collect();
                    }
                }
            }
            Ok(result)
        })
    }

    pub fn load_messages_by_run(&self, run_id: &str) -> Result<Vec<AgentMessage>, RepoError> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT message_id, run_id, conversation_id, seq, kind, role, blocks_json, created_at
                 FROM agent_messages WHERE run_id = ?1 ORDER BY created_at ASC, message_id ASC",
            )?;
            let rows = stmt
                .query_map(params![run_id], row_to_message)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn upsert_tool_call(&self, call: &ToolCall) -> Result<(), RepoError> {
        let input_summary = serde_json::to_string(&call.input_summary)?;
        let output_summary = match &call.output_summary {
            Some(v) => Some(serde_json::to_string(v)?),
            None => None,
        };
        let error_code = match call.error_code {
            Some(c) => Some(
                serde_json::to_value(c)?
                    .as_str()
                    .ok_or(RepoError::InvalidRow("error_code"))?
                    .to_string(),
            ),
            None => None,
        };
        self.db.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO agent_tool_calls (
                    tool_call_id, run_id, name,
                    input_summary_json, input_payload_ref,
                    output_summary_json, output_payload_ref,
                    is_error, error_code, started_at, ended_at, duration_ms
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![
                    call.tool_call_id,
                    call.run_id,
                    call.name,
                    input_summary,
                    call.input_payload_ref,
                    output_summary,
                    call.output_payload_ref,
                    call.is_error as i32,
                    error_code,
                    call.started_at.to_rfc3339(),
                    call.ended_at.map(|t| t.to_rfc3339()),
                    call.duration_ms.map(|d| d as i64),
                ],
            )?;
            Ok::<_, RepoError>(())
        })
    }

    /// 加载某 run 的全部 ToolCall（按 started_at 升序）。
    ///
    /// Spec: agent-infra-module.md §5 `load_tool_calls_by_run`。
    pub fn load_tool_calls_by_run(&self, run_id: &str) -> Result<Vec<ToolCall>, RepoError> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT tool_call_id, run_id, name,
                        input_summary_json, input_payload_ref,
                        output_summary_json, output_payload_ref,
                        is_error, error_code, started_at, ended_at, duration_ms
                 FROM agent_tool_calls WHERE run_id = ?1
                 ORDER BY started_at ASC, tool_call_id ASC",
            )?;
            let rows = stmt
                .query_map(params![run_id], row_to_tool_call)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn load_tool_call(&self, tool_call_id: &str) -> Result<Option<ToolCall>, RepoError> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT tool_call_id, run_id, name,
                        input_summary_json, input_payload_ref,
                        output_summary_json, output_payload_ref,
                        is_error, error_code, started_at, ended_at, duration_ms
                 FROM agent_tool_calls WHERE tool_call_id = ?1",
            )?;
            let row = stmt
                .query_row(params![tool_call_id], row_to_tool_call)
                .optional()?;
            Ok(row)
        })
    }
}

fn row_to_message(row: &Row) -> rusqlite::Result<AgentMessage> {
    let message_id: String = row.get(0)?;
    let run_id: Option<String> = row.get(1)?;
    let conversation_id: Option<String> = row.get(2)?;
    let seq: Option<i64> = row.get(3)?;
    let kind_s: Option<String> = row.get(4)?;
    let role_s: String = row.get(5)?;
    let blocks_json: String = row.get(6)?;
    let created_at_s: String = row.get(7)?;
    let role = serde_json::from_value(Json::String(role_s)).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let kind: Option<MessageKind> = match kind_s {
        Some(s) => Some(serde_json::from_value(Json::String(s)).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
        })?),
        None => None,
    };
    let blocks = serde_json::from_str(&blocks_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let created_at: DateTime<Utc> = DateTime::parse_from_rfc3339(&created_at_s)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);
    Ok(AgentMessage {
        message_id,
        run_id,
        conversation_id,
        seq,
        kind,
        role,
        blocks,
        created_at,
    })
}

fn row_to_tool_call(row: &Row) -> rusqlite::Result<ToolCall> {
    let tool_call_id: String = row.get(0)?;
    let run_id: String = row.get(1)?;
    let name: String = row.get(2)?;
    let input_summary_s: String = row.get(3)?;
    let input_payload_ref: Option<String> = row.get(4)?;
    let output_summary_s: Option<String> = row.get(5)?;
    let output_payload_ref: Option<String> = row.get(6)?;
    let is_error_i: i64 = row.get(7)?;
    let error_code_s: Option<String> = row.get(8)?;
    let started_at_s: String = row.get(9)?;
    let ended_at_s: Option<String> = row.get(10)?;
    let duration_ms_i: Option<i64> = row.get(11)?;

    let input_summary = serde_json::from_str(&input_summary_s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let output_summary = match output_summary_s {
        Some(s) => Some(serde_json::from_str::<Json>(&s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
        })?),
        None => None,
    };
    let error_code: Option<ErrorCode> = match error_code_s {
        Some(s) => Some(serde_json::from_value(Json::String(s)).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e))
        })?),
        None => None,
    };
    let started_at = DateTime::parse_from_rfc3339(&started_at_s)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(9, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);
    let ended_at = match ended_at_s {
        Some(s) => Some(
            DateTime::parse_from_rfc3339(&s)
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        10,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?
                .with_timezone(&Utc),
        ),
        None => None,
    };
    Ok(ToolCall {
        tool_call_id,
        run_id,
        name,
        input_summary,
        input_payload_ref,
        output_summary,
        output_payload_ref,
        is_error: is_error_i != 0,
        error_code,
        started_at,
        ended_at,
        duration_ms: duration_ms_i.map(|d| d as u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::{AgentMessageBlock, AgentMessageRole};
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::db::run_migrations;

    fn fresh_db() -> AppDb {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        db
    }

    #[test]
    fn message_round_trip() {
        let db = fresh_db();
        let repo = AgentMessagesRepo::new(db);
        let msg = AgentMessage {
            message_id: "m1".into(),
            run_id: Some("r1".into()),
            conversation_id: None,
            seq: None,
            kind: None,
            role: AgentMessageRole::Assistant,
            blocks: vec![AgentMessageBlock::Text { text: "hi".into() }],
            created_at: Utc::now(),
        };
        repo.upsert_message(&msg).unwrap();
        let loaded = repo.load_messages_by_run("r1").unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].message_id, "m1");
        assert_eq!(loaded[0].role, AgentMessageRole::Assistant);
    }

    fn conv_msg(id: &str, conv: &str, seq: i64, kind: Option<MessageKind>) -> AgentMessage {
        AgentMessage {
            message_id: id.into(),
            run_id: Some("r1".into()),
            conversation_id: Some(conv.into()),
            seq: Some(seq),
            kind,
            role: AgentMessageRole::Assistant,
            blocks: vec![AgentMessageBlock::Text {
                text: format!("msg {id}"),
            }],
            created_at: Utc::now(),
        }
    }

    #[test]
    fn next_seq_increments_per_conversation() {
        let db = fresh_db();
        let repo = AgentMessagesRepo::new(db);
        assert_eq!(repo.next_seq("conv-a").unwrap(), 0);
        repo.upsert_message(&conv_msg("m1", "conv-a", 0, None)).unwrap();
        assert_eq!(repo.next_seq("conv-a").unwrap(), 1);
        repo.upsert_message(&conv_msg("m2", "conv-a", 1, None)).unwrap();
        assert_eq!(repo.next_seq("conv-a").unwrap(), 2);
        // other conversation independent
        assert_eq!(repo.next_seq("conv-b").unwrap(), 0);
    }

    #[test]
    fn load_conversation_returns_all_in_seq_order() {
        let db = fresh_db();
        let repo = AgentMessagesRepo::new(db);
        // insert out of order
        repo.upsert_message(&conv_msg("m2", "c", 1, None)).unwrap();
        repo.upsert_message(&conv_msg("m0", "c", 0, None)).unwrap();
        repo.upsert_message(&conv_msg("m3", "c", 2, None)).unwrap();
        let all = repo.load_conversation("c").unwrap();
        let ids: Vec<&str> = all.iter().map(|m| m.message_id.as_str()).collect();
        assert_eq!(ids, vec!["m0", "m2", "m3"]);
    }

    #[test]
    fn load_conversation_view_returns_latest_summary_plus_after() {
        // Spec §5: view = latest summary checkpoint + all messages after it.
        let db = fresh_db();
        let repo = AgentMessagesRepo::new(db);
        repo.upsert_message(&conv_msg("m0", "c", 0, None)).unwrap();
        repo.upsert_message(&conv_msg("m1", "c", 1, None)).unwrap();
        repo.upsert_message(&conv_msg("s2", "c", 2, Some(MessageKind::Summary)))
            .unwrap();
        repo.upsert_message(&conv_msg("m3", "c", 3, None)).unwrap();
        repo.upsert_message(&conv_msg("m4", "c", 4, None)).unwrap();
        // full audit still returns everything
        assert_eq!(repo.load_conversation("c").unwrap().len(), 5);
        // view: summary s2 + m3 + m4
        let view = repo.load_conversation_view("c").unwrap();
        let ids: Vec<&str> = view.iter().map(|m| m.message_id.as_str()).collect();
        assert_eq!(ids, vec!["s2", "m3", "m4"]);
        assert_eq!(view[0].kind, Some(MessageKind::Summary));
    }

    #[test]
    fn load_conversation_view_no_summary_returns_all() {
        let db = fresh_db();
        let repo = AgentMessagesRepo::new(db);
        repo.upsert_message(&conv_msg("m0", "c", 0, None)).unwrap();
        repo.upsert_message(&conv_msg("m1", "c", 1, None)).unwrap();
        let view = repo.load_conversation_view("c").unwrap();
        assert_eq!(view.len(), 2);
    }

    #[test]
    fn tool_call_round_trip() {
        let db = fresh_db();
        let repo = AgentMessagesRepo::new(db);
        let tc = ToolCall {
            tool_call_id: "tc_1".into(),
            run_id: "r1".into(),
            name: "fetch_quote".into(),
            input_summary: serde_json::json!({"tsCode":"600519.SH"}),
            input_payload_ref: None,
            output_summary: Some(serde_json::json!({"price":"100.5"})),
            output_payload_ref: None,
            is_error: false,
            error_code: None,
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
            duration_ms: Some(45),
        };
        repo.upsert_tool_call(&tc).unwrap();
        let loaded = repo.load_tool_call("tc_1").unwrap().unwrap();
        assert_eq!(loaded.name, "fetch_quote");
        assert_eq!(loaded.is_error, false);
        assert_eq!(loaded.duration_ms, Some(45));
    }

    #[test]
    fn tool_call_with_error_code_round_trip() {
        let db = fresh_db();
        let repo = AgentMessagesRepo::new(db);
        let tc = ToolCall {
            tool_call_id: "tc_2".into(),
            run_id: "r1".into(),
            name: "operate_account".into(),
            input_summary: serde_json::json!({}),
            input_payload_ref: Some("pl_in".into()),
            output_summary: Some(serde_json::json!({"reason":"insufficient_cash"})),
            output_payload_ref: None,
            is_error: true,
            error_code: Some(ErrorCode::InsufficientCash),
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
            duration_ms: Some(7),
        };
        repo.upsert_tool_call(&tc).unwrap();
        let loaded = repo.load_tool_call("tc_2").unwrap().unwrap();
        assert_eq!(loaded.error_code, Some(ErrorCode::InsufficientCash));
        assert_eq!(loaded.input_payload_ref.as_deref(), Some("pl_in"));
    }

    /// Spec §4：durable（trading_write）消息即便 seq 落在最新 summary 之前，
    /// `load_conversation_view` 也必须把它包含进视图（跨 run 永不离开 LLM 视野）。
    #[test]
    fn view_includes_durable_messages_before_summary() {
        let db = fresh_db();
        let repo = AgentMessagesRepo::new(db);
        let conv = "c-durable";
        let mk = |id: &str, seq: i64, kind: Option<MessageKind>, text: &str| AgentMessage {
            message_id: id.into(),
            run_id: Some("r1".into()),
            conversation_id: Some(conv.into()),
            seq: Some(seq),
            kind,
            role: AgentMessageRole::User,
            blocks: vec![AgentMessageBlock::Text { text: text.into() }],
            created_at: Utc::now(),
        };
        repo.upsert_message(&mk("old", 0, None, "旧闲聊")).unwrap();
        repo.upsert_message(&mk(
            "trade",
            1,
            None,
            r#"<tool_result name="operate_account" call_id="tc_t">{"orderId":"o1"}</tool_result>"#,
        ))
        .unwrap();
        repo.mark_durable("trade").unwrap();
        repo.upsert_message(&mk("sum", 2, Some(MessageKind::Summary), "滚动摘要"))
            .unwrap();
        repo.upsert_message(&mk("tail", 3, None, "最近一轮")).unwrap();

        let durable = repo.load_durable_ids(conv).unwrap();
        assert_eq!(durable, vec!["trade".to_string()]);

        let view = repo.load_conversation_view(conv).unwrap();
        let ids: Vec<&str> = view.iter().map(|m| m.message_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["trade", "sum", "tail"],
            "视图 = summary 前的 durable 行 + summary + 其后；旧闲聊被挤出"
        );
    }
}
