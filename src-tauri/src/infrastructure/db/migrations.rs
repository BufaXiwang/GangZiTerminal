//! SQLite schema 单一来源（v3 expectation-driven）。
//!
//! 旧 DB 文件在 `connection::open_database` 启动时根据 SCHEMA_VERSION 比对自动备份
//! （`gangzi-terminal.sqlite3.legacy-{ts}`），本文件**只**负责在空 DB 上建一遍新 schema。
//! 不需要 in-place 升级、不需要 add_column_if_missing。
//!
//! 模块归属：
//! - **Account**: simulated_positions / position_events / expectations / expectation_events
//!                  （v2 残留：theses / thesis_codes / thesis_events——W23/W24 删旧 code 时一起去掉 CREATE TABLE）
//! - **Agent**: chat_messages / agent_episodes / agent_episode_turns / heuristics / strategies /
//!              strategy_events / lessons / signal_detections / news_tags / news_tickers
//!              （v2 残留：principles——W22 末迁移到 heuristics 后删除）
//! - **Quotes**: stocks / indexes / funds / klines / kline_meta / minute_klines / minute_kline_meta
//! - **News**: news_items / article_contents
//! - **系统**: schema_meta / app_state (KV)
//!
//! 注：simulated_positions 多了 `current_expectation_id` 列；agent_episodes 多了 `expectation_ids` 列。
//! v2 时代的 `thesis_id` / `thesis_ids` 列暂保留兼容旧代码——W23 切干净后 schema v4 删。

use crate::infrastructure::db::connection::SCHEMA_VERSION;
use crate::infrastructure::db::helpers::now;
use rusqlite::{params, Connection};

pub fn migrate(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(SCHEMA_SQL)
        .map_err(|err| format!("初始化 SQLite schema 失败：{err}"))?;
    connection
        .execute(
            "insert into schema_meta (id, version, updated_at)
             values (1, ?1, ?2)
             on conflict(id) do update set version = excluded.version, updated_at = excluded.updated_at",
            params![SCHEMA_VERSION, now()],
        )
        .map_err(|err| format!("写入 schema 版本失败：{err}"))?;
    Ok(())
}

const SCHEMA_SQL: &str = r#"
-- ===== 系统 =====
create table if not exists schema_meta (
    id integer primary key check (id = 1),
    version integer not null,
    updated_at text not null
);

create table if not exists app_state (
    key text primary key,
    value_json text not null,
    updated_at text not null
);

-- ===== News BC =====
create table if not exists news_items (
    id text primary key,
    source text not null,
    published text,
    payload_json text not null,
    created_at text not null,
    updated_at text not null
);

create table if not exists article_contents (
    url text primary key,
    item_id text,
    payload_json text not null,
    fetched_at text not null
);

-- ===== Account BC =====
create table if not exists simulated_positions (
    id text primary key,
    code text not null,
    source_analysis_id text not null,
    status text not null,
    current_expectation_id text,          -- v3：关联 expectations 表
    payload_json text not null,
    created_at text not null,
    updated_at text not null
);
create index if not exists idx_simulated_positions_code_status on simulated_positions(code, status);
create index if not exists idx_simulated_positions_expectation on simulated_positions(current_expectation_id);

create table if not exists position_events (
    id text primary key,
    position_id text not null,
    event_kind text not null,
    occurred_at text not null,
    source_kind text,
    source_ref text,
    payload_json text not null,
    agent_note_md text,
    created_at text not null
);
create index if not exists idx_position_events_pos_time on position_events(position_id, occurred_at);

-- ===== Agent BC =====
create table if not exists chat_messages (
    id text primary key,
    created_at text not null,
    role text not null check (role in ('user', 'assistant', 'system')),
    kind text not null check (kind in ('chat', 'system', 'highlight', 'compact_boundary')),
    content_md text not null,
    content_json text,
    source_task_id text,
    source_news_ids text,
    source_record_id text
);
create index if not exists idx_chat_messages_created on chat_messages(created_at desc);
create index if not exists idx_chat_messages_kind on chat_messages(kind, created_at desc);

-- agent_episodes：每次 agent run 一行。trigger_kind 区分 chat / scheduled / reflection 等
create table if not exists agent_episodes (
    run_id text primary key,
    trigger_kind text not null check (trigger_kind in ('scheduled', 'user_message', 'user_instruction', 'reflection', 'chat')),
    trigger_ref text,
    provider text not null,
    model text not null,
    started_at text not null,
    ended_at text,
    turns integer not null default 0,
    input_tokens integer not null default 0,
    output_tokens integer not null default 0,
    cache_read_tokens integer not null default 0,
    cache_write_tokens integer not null default 0,
    local_tool_calls integer not null default 0,
    server_tool_calls integer not null default 0,
    stop_reason text,
    error text,
    trigger_message_id text,
    expectation_ids text,                 -- v3：JSON array of ExpectationId
    outcome_summary text,
    parent_episode_id text                -- 因果链
);
create index if not exists idx_agent_episodes_trigger_started on agent_episodes(trigger_kind, started_at desc);
create index if not exists idx_agent_episodes_parent on agent_episodes(parent_episode_id);

create table if not exists agent_episode_turns (
    run_id text not null,
    turn integer not null,
    started_at text not null,
    ended_at text,
    stop_reason text,
    input_tokens integer not null default 0,
    output_tokens integer not null default 0,
    cache_read_tokens integer not null default 0,
    local_tool_calls integer not null default 0,
    server_tool_calls integer not null default 0,
    error text,
    primary key (run_id, turn)
);
create index if not exists idx_agent_episode_turns_run on agent_episode_turns(run_id, turn);

-- ===== Quotes BC =====
create table if not exists stocks (
    code text primary key,
    name text not null,
    sector text,
    market text not null,
    updated_at text not null
);
create index if not exists idx_stocks_name on stocks(name);

create table if not exists indexes (
    ts_code text primary key,
    code text not null,
    name text not null,
    market text not null,
    publisher text,
    category text,
    updated_at text not null
);
create index if not exists idx_indexes_name on indexes(name);

create table if not exists funds (
    ts_code text primary key,
    code text not null,
    name text not null,
    market text not null,
    fund_type text,
    management text,
    list_date text,
    status text,
    updated_at text not null
);
create index if not exists idx_funds_name on funds(name);
create index if not exists idx_funds_market on funds(market);

create table if not exists klines (
    ts_code text not null,
    period text not null,
    adjust text not null,
    date text not null,
    open real not null,
    close real not null,
    high real not null,
    low real not null,
    volume real,
    amount real,
    source text not null,
    primary key (ts_code, period, adjust, date)
);
create index if not exists idx_klines_ts_period_date on klines(ts_code, period, adjust, date desc);

create table if not exists kline_meta (
    ts_code text not null,
    period text not null,
    adjust text not null,
    last_known_date text not null,
    fetched_at text not null,
    primary key (ts_code, period, adjust)
);

create table if not exists minute_klines (
    ts_code text not null,
    period text not null,
    timestamp_ms integer not null,
    open real not null,
    close real not null,
    high real not null,
    low real not null,
    volume integer not null,
    amount real not null,
    source text not null,
    primary key (ts_code, period, timestamp_ms)
);
create index if not exists idx_minute_klines_ts_period_ts on minute_klines(ts_code, period, timestamp_ms desc);

create table if not exists minute_kline_meta (
    ts_code text not null,
    period text not null,
    last_known_ts integer not null,
    fetched_at text not null,
    primary key (ts_code, period)
);

-- ===== v3 expectation-driven 新表 =====

-- Expectation：投资预期一等聚合根（归 account BC）
create table if not exists expectations (
    id text primary key,
    code text not null,
    direction text not null check (direction in ('up', 'down', 'range_bound')),
    target_price real,
    target_price_ceiling real,
    horizon_days integer not null,
    reasoning text not null,
    signals_used text not null,            -- JSON array of SignalKind
    conviction text not null check (conviction in ('low', 'medium', 'high')),
    theme text,
    supersedes_expectation_id text,
    state text not null check (state in ('pending', 'hit', 'missed', 'expired', 'cancelled', 'superseded')),
    regime_at_creation text,
    created_at text not null,
    expires_at text not null,
    closed_at text
);
create index if not exists idx_expectations_code_state on expectations(code, state);
create index if not exists idx_expectations_state_expires on expectations(state, expires_at);
create index if not exists idx_expectations_theme on expectations(theme);

-- Expectation 事件链（状态机审计 + 用户反馈 append-only）
create table if not exists expectation_events (
    id integer primary key autoincrement,
    expectation_id text not null,
    kind text not null,
    payload text,                          -- JSON
    occurred_at text not null
);
create index if not exists idx_expectation_events_id on expectation_events(expectation_id, occurred_at);

-- Strategy：用户 + agent 共建的规则集
create table if not exists strategies (
    id text primary key,
    name text not null,
    description text,
    config_json text not null,             -- 完整 DSL（trigger_when + target + conviction_rule）
    enabled integer not null default 1,
    applied_count integer not null default 0,
    hit_count integer not null default 0,
    miss_count integer not null default 0,
    created_at text not null,
    updated_at text not null
);
create index if not exists idx_strategies_enabled on strategies(enabled);

-- Strategy 修改审计
create table if not exists strategy_events (
    id integer primary key autoincrement,
    strategy_id text not null,
    kind text not null,                    -- created/updated/enabled/disabled/user_comment
    payload text,                          -- JSON
    occurred_at text not null
);
create index if not exists idx_strategy_events_id on strategy_events(strategy_id, occurred_at);

-- Lesson：每个 expectation 终态自动生成的原子观察（学习闭环底层原料）
create table if not exists lessons (
    id text primary key,
    expectation_id text not null,
    code text not null,
    observation text not null,
    takeaway text not null,
    outcome text not null check (outcome in ('hit', 'miss', 'expired')),
    regime_at_close text,
    signals_in_play text,                  -- JSON array
    pnl_pct real,
    created_at text not null
);
create index if not exists idx_lessons_expectation on lessons(expectation_id);
create index if not exists idx_lessons_code_time on lessons(code, created_at desc);

-- Heuristic：结构化启发式规则 + track record（取代 v2 principles）
create table if not exists heuristics (
    id text primary key,
    body text not null,
    category text not null check (category in ('principle', 'known_bias', 'risk_preference')),
    origin text not null check (origin in ('seed', 'user_stated', 'agent_inferred')),
    regime_tags text,                      -- JSON array
    supporting_lesson_ids text,            -- JSON array
    application_count integer not null default 0,
    hit_count integer not null default 0,
    miss_count integer not null default 0,
    last_applied_at text,
    retired_at text,
    retired_reason text,
    created_at text not null
);
create index if not exists idx_heuristics_origin on heuristics(origin);
create index if not exists idx_heuristics_retired on heuristics(retired_at);

-- Signal detection log（per-tick 检测结果，审计 + 命中率统计）
create table if not exists signal_detections (
    id integer primary key autoincrement,
    tick_id text not null,
    code text not null,
    signal_family text not null,           -- 稳定 key（无参数），便于按家族聚合
    signal_json text not null,             -- 完整 SignalKind 序列化（含参数）
    detected_at text not null
);
create index if not exists idx_signal_detections_code_time on signal_detections(code, detected_at desc);
create index if not exists idx_signal_detections_tick on signal_detections(tick_id);
create index if not exists idx_signal_detections_family on signal_detections(signal_family, detected_at desc);

-- News tagger 输出（资讯入库时打 kind/importance/tickers/sectors）
create table if not exists news_tags (
    news_id text primary key,
    kind text not null check (kind in ('earnings','halt','restructure','regulatory','ownership','operating','policy','sector_trend','market','other')),
    importance text not null check (importance in ('high','medium','low')),
    sectors text,                          -- JSON array
    tagged_at text not null
);
create index if not exists idx_news_tags_importance on news_tags(importance);

create table if not exists news_tickers (
    news_id text not null,
    code text not null,
    primary key (news_id, code)
);
create index if not exists idx_news_tickers_code on news_tickers(code, news_id);
"#;
