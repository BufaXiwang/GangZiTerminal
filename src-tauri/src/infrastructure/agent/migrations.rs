//! Agent Infra schema migrations。
//!
//! Spec: docs/design/agent-infra-module.md §2（不变量：ToolCall 审计、message 持久化）
//!
//! 表前缀 `agent_*`（AGENTS.md 分区约束）。
//!
//! 真源表：
//! - `agent_messages`           — `AgentMessage` 持久化对话 / context 消息（spec §2）
//! - `agent_tool_calls`        — `ToolCall` 审计（spec §2 `ToolCall` 不变量）
//! - `agent_provider_channels`  — `ProviderChannel` 配置（spec §2 `ProviderChannel`）
//! - `agent_payloads`           — PayloadStore 完整 payload（spec §2 PayloadStore）

use rusqlite_migration::M;

/// 返回 Agent Infra BC 的所有迁移，按版本顺序排列。
///
/// **append-only**：新增 migration 只能加到末尾（rusqlite_migration 按全局位置判定 user_version）。
pub fn migrations() -> Vec<M<'static>> {
    let mut all = migrations_base();
    all.extend(migrations_tail());
    all
}

/// 全局组合用的「基础段」（全局序号 6–11，**冻结不可再增**）。
/// 新增 agent migration 一律加 `migrations_tail()`（拼在全局列表末尾，append-only）。
pub fn migrations_base() -> Vec<M<'static>> {
    vec![
        M::up(MIGRATION_001_INITIAL),
        M::up(MIGRATION_002_CHANNEL_AUTH_ACTIVE),
        M::up(MIGRATION_003_MESSAGES_CONVERSATION),
        M::up(MIGRATION_004_RUNTIME),
        M::up(MIGRATION_005_SETTINGS),
        M::up(MIGRATION_006_REVIEW_FOLLOWUP),
    ]
}

/// 全局**尾部**追加段（在 `all_migrations()` 末尾拼接；不得插进基础段——会顶掉
/// 后续 BC 的全局序号，使存量 DB 重跑建表 panic）。
pub fn migrations_tail() -> Vec<M<'static>> {
    vec![M::up(MIGRATION_TAIL_001_MESSAGE_DURABLE)]
}

// Spec: agent-infra-module.md §4 上下文管理（trading_write / durable 永不丢）
//
// durable 标记持久化：trading_write 轮的 tool_result user message 在落库时打标，
// 使「永不压缩 / 替 stub / 不折进摘要」跨 run 续接仍生效（load 视图必含 durable 行 +
// loop 续接时据此重建 durable_message_ids）。kind=summary 的 durable 性由 kind 本身表达，不用此列。
const MIGRATION_TAIL_001_MESSAGE_DURABLE: &str = r#"
ALTER TABLE agent_messages ADD COLUMN durable INTEGER NOT NULL DEFAULT 0;
"#;

// Spec: agent-runtime-module.md §3（复盘报告 ④ follow-up + ② 基准对照「组合收益」）
//
// 两张确定性复盘对账表（都 append-only / 由 Runtime 写，review agent 不直接写）：
//
// - agent_review_suggestions: 持久化 ReviewSuggestion——review run 经 `record_review_suggestion`
//   工具声明的策略建议 `{suggestionId, reviewRunId, tradeDate, text, createdAt}`。下次 review 读
//   上一交易日的建议，对账「是否已经 upsert 采纳」（查策略版本历史的时间）→ follow-up 段确定性写出。
//
// - agent_daily_equity（**已废弃 / DEPRECATED，不再读写**）：原 Runtime 侧当日「日初权益」基线表。
//   该计算已下沉到 Account（账户财务事实单一所有者，spec account-module.md §2「账户财务事实只读
//   facade」）—— 新表 `account_day_equity`（Account 拥有，物理放全局拼接末尾，见
//   `account::migrations_tail()`）+ `AccountService::daily_return`。本历史 migration 因 append-only
//   约束保留建表（删历史 migration 会移位破坏存量 DB），但 Runtime 代码不再读写它。
const MIGRATION_006_REVIEW_FOLLOWUP: &str = r#"
CREATE TABLE agent_review_suggestions (
    suggestion_id  TEXT PRIMARY KEY,
    review_run_id  TEXT NOT NULL,
    trade_date     TEXT NOT NULL,            -- YYYYMMDD（CN 交易日）
    text           TEXT NOT NULL,            -- 策略建议文本
    created_at     TEXT NOT NULL
);
CREATE INDEX idx_agent_review_suggestions_date ON agent_review_suggestions (trade_date, created_at);

CREATE TABLE agent_daily_equity (
    trade_date   TEXT PRIMARY KEY,           -- YYYYMMDD（CN 交易日）
    open_equity  TEXT NOT NULL,              -- 当日日初权益（Decimal as string）
    captured_at  TEXT NOT NULL
);
"#;

// Spec: agent-runtime-module.md §8 Runtime settings keys
//
// agent_settings: 运行时配置 kv 真源（settings store）。所有硬编码阈值改读此表，
// 缺失用缺省、解析非法 fail-closed（退回缺省）+ heartbeat（tracing::warn）。
const MIGRATION_005_SETTINGS: &str = r#"
CREATE TABLE agent_settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
"#;

// Spec: agent-runtime-module.md §3 领域模型 / §8 幂等与可靠性
//
// Runtime 真源表（都挂 run_id 决策链）：
// - agent_runs              — AgentRun 生命周期（决策链主键）
// - agent_investment_strategy — InvestmentStrategy 版本化（单点写）
// - agent_analysis_results  — AnalysisResult（news 分析产物）
// - agent_trades            — AgentTrade 审计戳（submitting→settled）
// - agent_order_run_index   — orderId → runId 反查（account_trigger 归因）
// - agent_news_buffer       — news 待分析 buffer（4h 滚动 / drain / age-out）
// - agent_event_consumption — 跨 BC 事件消费幂等
// - agent_heartbeats        — 后台 loop 健康
const MIGRATION_004_RUNTIME: &str = r#"
CREATE TABLE agent_runs (
    run_id           TEXT PRIMARY KEY,
    mode             TEXT NOT NULL,          -- dialogue | news | account_trigger | review
    trigger_json     TEXT NOT NULL,          -- AgentRunTrigger
    parent_run_id    TEXT,                   -- review 被 fork 时指向父 run
    provider         TEXT NOT NULL,
    wire_format      TEXT NOT NULL,
    model            TEXT NOT NULL,
    strategy_version INTEGER,                -- 创建时冻结的 active 策略版本
    causation_run_id TEXT,                   -- account_trigger 归因到建仓 run
    status           TEXT NOT NULL,          -- queued | running | completed | failed | cancelled
    started_at       TEXT,
    ended_at         TEXT,
    error            TEXT,
    created_at       TEXT NOT NULL
);
CREATE INDEX idx_agent_runs_status ON agent_runs (status);
CREATE INDEX idx_agent_runs_parent ON agent_runs (parent_run_id);

CREATE TABLE agent_investment_strategy (
    strategy_id  TEXT NOT NULL,
    version      INTEGER NOT NULL,
    strategy     TEXT NOT NULL,             -- 自然语言
    status       TEXT NOT NULL,             -- active | paused
    reason       TEXT,                      -- 本次写入理由（审计）
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    PRIMARY KEY (strategy_id, version)
);
CREATE INDEX idx_agent_strategy_status ON agent_investment_strategy (status, version);

CREATE TABLE agent_analysis_results (
    result_id      TEXT PRIMARY KEY,
    run_id         TEXT NOT NULL,
    kind           TEXT NOT NULL,           -- action | no_action
    summary        TEXT NOT NULL,
    related_codes_json TEXT NOT NULL,       -- TsCode[]
    trade_ids_json TEXT NOT NULL DEFAULT '[]',
    created_at     TEXT NOT NULL
);
CREATE INDEX idx_agent_analysis_run ON agent_analysis_results (run_id);
CREATE INDEX idx_agent_analysis_created ON agent_analysis_results (created_at);

CREATE TABLE agent_trades (
    trade_id              TEXT PRIMARY KEY,
    run_id                TEXT NOT NULL,
    client_order_id       TEXT NOT NULL,
    strategy_version      INTEGER,
    reason                TEXT NOT NULL,
    account_input_summary TEXT NOT NULL,
    status                TEXT NOT NULL,     -- submitting | settled
    account_result_json   TEXT,              -- AccountResultRef（settled 后填）
    created_at            TEXT NOT NULL,
    updated_at            TEXT NOT NULL
);
CREATE INDEX idx_agent_trades_run ON agent_trades (run_id);
CREATE INDEX idx_agent_trades_status ON agent_trades (status);
CREATE UNIQUE INDEX idx_agent_trades_client_order ON agent_trades (client_order_id);

-- orderId → runId 反查：account_trigger run 据此归因到原始建仓 run。
-- accepted=true 且有 orderId 时写入。
CREATE TABLE agent_order_run_index (
    order_id        TEXT PRIMARY KEY,
    run_id          TEXT NOT NULL,
    trade_id        TEXT NOT NULL,
    client_order_id TEXT NOT NULL,
    created_at      TEXT NOT NULL
);

-- news 待分析 buffer：4h 滚动未分析队列（drain / newest-first / age-out）。
CREATE TABLE agent_news_buffer (
    news_id      TEXT PRIMARY KEY,
    entered_at   TEXT NOT NULL,             -- 入队时间（age-out 依据）
    published_at TEXT,                      -- 排序依据（newest-first）
    status       TEXT NOT NULL,             -- pending | in_batch | analyzed | dropped
    run_id       TEXT                       -- 进入某批分析后写
);
CREATE INDEX idx_agent_news_buffer_status ON agent_news_buffer (status, published_at);

-- 跨 BC 事件消费幂等：(event_type, event_key, consumer) 为幂等键。
CREATE TABLE agent_event_consumption (
    event_type  TEXT NOT NULL,
    event_key   TEXT NOT NULL,
    consumer    TEXT NOT NULL,
    status      TEXT NOT NULL,              -- processing | consumed | ignored | failed
    run_id      TEXT,
    error       TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (event_type, event_key, consumer)
);

-- 后台 loop 心跳（可观测）。
CREATE TABLE agent_heartbeats (
    loop_name            TEXT PRIMARY KEY,
    last_ok_at           TEXT,
    last_error_at        TEXT,
    last_error           TEXT,
    consecutive_failures INTEGER NOT NULL DEFAULT 0
);
"#;

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

-- agent_tool_calls: ToolCall 审计
CREATE TABLE agent_tool_calls (
    tool_call_id       TEXT PRIMARY KEY,
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

CREATE INDEX idx_agent_tool_calls_run
    ON agent_tool_calls (run_id, started_at);

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
-- 写入触发：tool input/output > 8KB；image 任何尺寸
-- 第一阶段不实现 GC，永久保留供 decision episode replay。
CREATE TABLE agent_payloads (
    payload_id   TEXT PRIMARY KEY,                -- pl_<uuid>
    kind         TEXT NOT NULL,                   -- tool_input | tool_output | image
    content_json TEXT,                            -- JSON payload (kind = tool_input|tool_output)
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
            "INSERT INTO agent_tool_calls
              (tool_call_id, run_id, name, input_summary_json, is_error, started_at)
             VALUES ('tc_1', 'r1', 'fetch_quote', '{}', 0, '2026-05-27T01:00:00Z')",
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
             VALUES ('pl_1', 'tool_output', '{\"k\":1}', 7, '2026-05-27T01:00:00Z')",
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
    fn migration_005_settings_kv() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn, migrations()).unwrap();
        conn.execute(
            "INSERT INTO agent_settings (key, value, updated_at)
             VALUES ('news_agent_batch_size', '50', '2026-05-27T01:00:00Z')",
            [],
        )
        .unwrap();
        let v: String = conn
            .query_row(
                "SELECT value FROM agent_settings WHERE key = 'news_agent_batch_size'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, "50");
    }

    #[test]
    fn migration_006_review_followup_tables() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn, migrations()).unwrap();
        conn.execute(
            "INSERT INTO agent_review_suggestions
              (suggestion_id, review_run_id, trade_date, text, created_at)
             VALUES ('rs_1', 'rev1', '20260604', '收紧单票上限', '2026-06-04T07:30:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agent_daily_equity (trade_date, open_equity, captured_at)
             VALUES ('20260605', '1000000', '2026-06-05T01:30:00Z')",
            [],
        )
        .unwrap();
        let text: String = conn
            .query_row(
                "SELECT text FROM agent_review_suggestions WHERE suggestion_id='rs_1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(text, "收紧单票上限");
        let eq: String = conn
            .query_row(
                "SELECT open_equity FROM agent_daily_equity WHERE trade_date='20260605'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(eq, "1000000");
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
