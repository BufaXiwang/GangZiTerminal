//! Agent Infra schema migrations。
//!
//! Spec: docs/design/agent-infra-module.md §2（不变量：tool_call 审计、message 持久化）
//!
//! 表前缀 `agent_*`（AGENTS.md 分区约束）。
//!
//! 真源表：
//! - `agent_messages`           — `AgentMessage` 持久化对话 / context 消息（spec §2）
//! - `agent_tool_calls`         — `ToolCall` 审计（spec §2 `ToolCall` 不变量）
//! - `agent_provider_channels`  — `ProviderChannel` 配置（spec §2 `ProviderChannel`）
//!
//! 设计约束：
//! - JSON blob 列存放 block / summary / payload；具体序列化由 repo 层负责。
//! - tool_call_id 在 messages（block 内）和 agent_tool_calls 之间是冗余 join key，
//!   不强制外键，保证两表写入顺序可独立。

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
    role         TEXT NOT NULL,       -- system | user | assistant | tool
    blocks_json  TEXT NOT NULL,       -- AgentMessageBlock[]
    created_at   TEXT NOT NULL        -- ISO-8601 UTC
);

CREATE INDEX idx_agent_messages_run_created
    ON agent_messages (run_id, created_at);

-- agent_tool_calls: ToolCall 审计
CREATE TABLE agent_tool_calls (
    tool_call_id        TEXT PRIMARY KEY,
    run_id              TEXT NOT NULL,
    name                TEXT NOT NULL,
    source              TEXT NOT NULL,        -- local_tool | server_side_tool
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

CREATE INDEX idx_agent_tool_calls_run
    ON agent_tool_calls (run_id, started_at);

-- agent_provider_channels: ProviderChannel 配置
CREATE TABLE agent_provider_channels (
    channel_id                  TEXT PRIMARY KEY,
    provider                    TEXT NOT NULL,
    wire_format                 TEXT NOT NULL,        -- messages | responses | chat_completions
    base_url                    TEXT,
    model                       TEXT NOT NULL,
    stream                      INTEGER NOT NULL DEFAULT 1,
    supports_tools              INTEGER NOT NULL DEFAULT 1,
    supports_vision             INTEGER NOT NULL DEFAULT 0,
    supports_thinking           INTEGER NOT NULL DEFAULT 0,
    server_side_tools_json      TEXT,                 -- Vec<String>，nullable
    created_at                  TEXT NOT NULL,
    updated_at                  TEXT NOT NULL
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
            "INSERT INTO agent_tool_calls
              (tool_call_id, run_id, name, source, input_summary_json, is_error, started_at)
             VALUES ('tc1', 'r1', 'fetch_quote', 'local_tool', '{}', 0, '2026-05-27T01:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agent_provider_channels
              (channel_id, provider, wire_format, model, stream, supports_tools,
               supports_vision, supports_thinking, created_at, updated_at)
             VALUES ('ch1', 'anthropic', 'messages', 'claude-sonnet-4-5', 1, 1, 0, 0,
                     '2026-05-27T01:00:00Z', '2026-05-27T01:00:00Z')",
            [],
        )
        .unwrap();
    }
}
