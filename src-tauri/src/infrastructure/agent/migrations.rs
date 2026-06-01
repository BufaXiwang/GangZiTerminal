//! Agent Infra schema migrations。
//!
//! Spec: docs/design/agent-infra-module.md §2（不变量：SkillCall 审计、message 持久化）
//!
//! 表前缀 `agent_*`（AGENTS.md 分区约束）。
//!
//! 真源表：
//! - `agent_messages`           — `AgentMessage` 持久化对话 / context 消息（spec §2）
//! - `agent_skill_calls`        — `SkillCall` 审计（spec §2 `SkillCall` 不变量）
//! - `agent_provider_channels`  — `ProviderChannel` 配置（spec §2 `ProviderChannel`）
//! - `agent_payloads`           — PayloadStore 完整 payload（spec §2 PayloadStore）

use rusqlite_migration::M;

/// 返回 Agent Infra BC 的所有迁移，按版本顺序排列。
///
/// **append-only**：新增 migration 只能加到末尾（rusqlite_migration 按全局位置判定 user_version）。
pub fn migrations() -> Vec<M<'static>> {
    vec![
        M::up(MIGRATION_001_INITIAL),
        M::up(MIGRATION_002_CHANNEL_AUTH_ACTIVE),
        M::up(MIGRATION_003_MESSAGES_CONVERSATION),
    ]
}

// Spec: agent-infra-module.md §2
const MIGRATION_001_INITIAL: &str = r#"
-- agent_messages: 持久化对话 / context 消息
CREATE TABLE agent_messages (
    message_id   TEXT PRIMARY KEY,
    run_id       TEXT,
    role         TEXT NOT NULL,       -- system | user | assistant
    blocks_json  TEXT NOT NULL,       -- AgentMessageBlock[]
    created_at   TEXT NOT NULL        -- ISO-8601 UTC
);

CREATE INDEX idx_agent_messages_run_created
    ON agent_messages (run_id, created_at);

-- agent_skill_calls: SkillCall 审计
CREATE TABLE agent_skill_calls (
    skill_call_id       TEXT PRIMARY KEY,
    run_id              TEXT NOT NULL,
    name                TEXT NOT NULL,
    input_summary_json  TEXT NOT NULL,
    input_payload_ref   TEXT,
    output_summary_json TEXT,
    output_payload_ref  TEXT,
    is_error            INTEGER NOT NULL DEFAULT 0,
    error_code          TEXT,
    started_at          TEXT NOT NULL,
    ended_at            TEXT,
    duration_ms         INTEGER
);

CREATE INDEX idx_agent_skill_calls_run
    ON agent_skill_calls (run_id, started_at);

-- agent_provider_channels: ProviderChannel 配置
CREATE TABLE agent_provider_channels (
    channel_id              TEXT PRIMARY KEY,
    provider                TEXT NOT NULL,
    wire_format             TEXT NOT NULL,        -- messages | responses | chat_completions
    base_url                TEXT,
    model                   TEXT NOT NULL,
    stream                  INTEGER NOT NULL DEFAULT 1,
    supports_vision         INTEGER NOT NULL DEFAULT 0,
    supports_thinking       INTEGER NOT NULL DEFAULT 0,
    max_output_tokens       INTEGER,
    context_window_tokens   INTEGER,
    created_at              TEXT NOT NULL,
    updated_at              TEXT NOT NULL
);

-- agent_payloads: PayloadStore 完整 input / output / image 副本
-- 写入触发：skill input/output > 8KB；image 任何尺寸
-- 第一阶段不实现 GC，永久保留供 decision episode replay。
CREATE TABLE agent_payloads (
    payload_id   TEXT PRIMARY KEY,                -- pl_<uuid>
    kind         TEXT NOT NULL,                   -- skill_input | skill_output | image
    content_json TEXT,                            -- JSON payload (kind = skill_input|skill_output)
    content_bytes BLOB,                           -- image bytes (kind = image)
    content_type TEXT,                            -- mime (kind = image)
    byte_size    INTEGER NOT NULL,
    created_at   TEXT NOT NULL
);
"#;

// Spec: agent-infra-module.md §2 `ProviderChannel`（apiKey / enabled / active 标记）
const MIGRATION_002_CHANNEL_AUTH_ACTIVE: &str = r#"
ALTER TABLE agent_provider_channels ADD COLUMN api_key TEXT NOT NULL DEFAULT '';
ALTER TABLE agent_provider_channels ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;
ALTER TABLE agent_provider_channels ADD COLUMN is_active INTEGER NOT NULL DEFAULT 0;
"#;

// Spec: agent-infra-module.md §2 `AgentMessage` (conversationId / seq / kind)，§4 多轮会话持久化与续接
//
// 多轮会话支持：agent_messages 增加 conversation_id（会话分组）、seq（会话内单调序号、排序用）、
// kind（chat | summary，summary = §4 压缩检查点）。三列均可空（NULL），全量审计真源不变。
//
// **append-only / global-last**：agent migrations 在 lib.rs 里被 `all.extend(agent_migrations())`
// 最后拼接，故 agent-003 落在全局最后一位（已存在 DB user_version=6 → 只 apply 003）。
const MIGRATION_003_MESSAGES_CONVERSATION: &str = r#"
ALTER TABLE agent_messages ADD COLUMN conversation_id TEXT;
ALTER TABLE agent_messages ADD COLUMN seq INTEGER;
ALTER TABLE agent_messages ADD COLUMN kind TEXT;

CREATE INDEX idx_agent_messages_conversation_seq
    ON agent_messages (conversation_id, seq);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::db::run_migrations;
    use rusqlite::Connection;

    #[test]
    fn applies_initial_migration() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn, migrations()).unwrap();
        conn.execute(
            "INSERT INTO agent_messages (message_id, run_id, role, blocks_json, created_at)
             VALUES ('m1', 'r1', 'assistant', '[]', '2026-05-27T01:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agent_skill_calls
              (skill_call_id, run_id, name, input_summary_json, is_error, started_at)
             VALUES ('sc_1', 'r1', 'fetch_quote', '{}', 0, '2026-05-27T01:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agent_provider_channels
              (channel_id, provider, wire_format, model, stream,
               supports_vision, supports_thinking, created_at, updated_at)
             VALUES ('ch1', 'anthropic', 'messages', 'claude-sonnet-4-5', 1, 0, 0,
                     '2026-05-27T01:00:00Z', '2026-05-27T01:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agent_payloads (payload_id, kind, content_json, byte_size, created_at)
             VALUES ('pl_1', 'skill_output', '{\"k\":1}', 7, '2026-05-27T01:00:00Z')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn migration_002_adds_auth_active_columns() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn, migrations()).unwrap();
        conn.execute(
            "INSERT INTO agent_provider_channels
              (channel_id, provider, wire_format, model, stream, enabled, is_active,
               supports_vision, supports_thinking, api_key, created_at, updated_at)
             VALUES ('ch1', 'DeepSeek', 'chat_completions', 'deepseek-chat', 1, 1, 1, 0, 0,
                     'sk-secret', '2026-05-27T01:00:00Z', '2026-05-27T01:00:00Z')",
            [],
        )
        .unwrap();
        let (api_key, enabled, is_active): (String, i64, i64) = conn
            .query_row(
                "SELECT api_key, enabled, is_active FROM agent_provider_channels WHERE channel_id = 'ch1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(api_key, "sk-secret");
        assert_eq!(enabled, 1);
        assert_eq!(is_active, 1);
    }

    #[test]
    fn migration_003_adds_conversation_columns() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn, migrations()).unwrap();
        conn.execute(
            "INSERT INTO agent_messages
              (message_id, run_id, conversation_id, seq, kind, role, blocks_json, created_at)
             VALUES ('m1', 'r1', 'conv-1', 0, 'summary', 'assistant', '[]', '2026-05-27T01:00:00Z')",
            [],
        )
        .unwrap();
        let (conv, seq, kind): (String, i64, String) = conn
            .query_row(
                "SELECT conversation_id, seq, kind FROM agent_messages WHERE message_id = 'm1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(conv, "conv-1");
        assert_eq!(seq, 0);
        assert_eq!(kind, "summary");
    }
}
