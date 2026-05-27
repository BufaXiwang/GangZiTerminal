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
pub fn migrations() -> Vec<M<'static>> {
    vec![M::up(MIGRATION_001_INITIAL)]
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
}
