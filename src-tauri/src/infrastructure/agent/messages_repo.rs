//! Agent message / SkillCall 持久化。
//!
//! Spec: docs/design/agent-infra-module.md §2 不变量
//! - 所有 skill 调用必须先通过 `SkillRegistry` 校验。
//! - 所有 skill 调用都必须记录 `SkillCall`。

use crate::domain::agent::{AgentMessage, SkillCall};
use crate::domain::shared::ErrorCode;
use crate::infrastructure::db::AppDb;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension, Row};
use serde_json::Value as Json;

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
        self.db.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO agent_messages
                  (message_id, run_id, role, blocks_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    msg.message_id,
                    msg.run_id,
                    role,
                    blocks_json,
                    msg.created_at.to_rfc3339()
                ],
            )?;
            Ok::<_, RepoError>(())
        })
    }

    pub fn load_messages_by_run(&self, run_id: &str) -> Result<Vec<AgentMessage>, RepoError> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT message_id, run_id, role, blocks_json, created_at
                 FROM agent_messages WHERE run_id = ?1 ORDER BY created_at ASC, message_id ASC",
            )?;
            let rows = stmt
                .query_map(params![run_id], row_to_message)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn upsert_skill_call(&self, call: &SkillCall) -> Result<(), RepoError> {
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
                "INSERT OR REPLACE INTO agent_skill_calls (
                    skill_call_id, run_id, name,
                    input_summary_json, input_payload_ref,
                    output_summary_json, output_payload_ref,
                    is_error, error_code, started_at, ended_at, duration_ms
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![
                    call.skill_call_id,
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

    pub fn load_skill_call(&self, skill_call_id: &str) -> Result<Option<SkillCall>, RepoError> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT skill_call_id, run_id, name,
                        input_summary_json, input_payload_ref,
                        output_summary_json, output_payload_ref,
                        is_error, error_code, started_at, ended_at, duration_ms
                 FROM agent_skill_calls WHERE skill_call_id = ?1",
            )?;
            let row = stmt
                .query_row(params![skill_call_id], row_to_skill_call)
                .optional()?;
            Ok(row)
        })
    }
}

fn row_to_message(row: &Row) -> rusqlite::Result<AgentMessage> {
    let message_id: String = row.get(0)?;
    let run_id: Option<String> = row.get(1)?;
    let role_s: String = row.get(2)?;
    let blocks_json: String = row.get(3)?;
    let created_at_s: String = row.get(4)?;
    let role = serde_json::from_value(Json::String(role_s)).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let blocks = serde_json::from_str(&blocks_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let created_at: DateTime<Utc> = DateTime::parse_from_rfc3339(&created_at_s)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);
    Ok(AgentMessage {
        message_id,
        run_id,
        role,
        blocks,
        created_at,
    })
}

fn row_to_skill_call(row: &Row) -> rusqlite::Result<SkillCall> {
    let skill_call_id: String = row.get(0)?;
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
    Ok(SkillCall {
        skill_call_id,
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

    #[test]
    fn skill_call_round_trip() {
        let db = fresh_db();
        let repo = AgentMessagesRepo::new(db);
        let tc = SkillCall {
            skill_call_id: "sc_1".into(),
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
        repo.upsert_skill_call(&tc).unwrap();
        let loaded = repo.load_skill_call("sc_1").unwrap().unwrap();
        assert_eq!(loaded.name, "fetch_quote");
        assert_eq!(loaded.is_error, false);
        assert_eq!(loaded.duration_ms, Some(45));
    }

    #[test]
    fn skill_call_with_error_code_round_trip() {
        let db = fresh_db();
        let repo = AgentMessagesRepo::new(db);
        let tc = SkillCall {
            skill_call_id: "sc_2".into(),
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
        repo.upsert_skill_call(&tc).unwrap();
        let loaded = repo.load_skill_call("sc_2").unwrap().unwrap();
        assert_eq!(loaded.error_code, Some(ErrorCode::InsufficientCash));
        assert_eq!(loaded.input_payload_ref.as_deref(), Some("pl_in"));
    }
}
