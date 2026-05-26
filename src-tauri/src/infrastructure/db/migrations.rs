//! SQLite schema 单一来源。
//!
//! 旧 DB 文件在 `connection::open_database` 启动时根据 SCHEMA_VERSION 比对自动备份
//! （`gangzi-terminal.sqlite3.legacy-{ts}`），本文件**只**负责在空 DB 上建一遍新 schema。
//! 不需要 in-place 升级、不需要 add_column_if_missing。
//!
//! 模块归属：
//! - **Account**: simulated_positions / position_events
//! - **Agent Infra**: chat_messages / agent_episodes / agent_episode_turns
//! - **Agent Runtime**: agent_runs / decision_episodes / trade_intents /
//!   agent_order_intent_index / decision_reviews / strategy_cards /
//!   agent_event_consumption / agent_news_buffer
//! - **Quotes**: stocks / indexes / funds / klines / kline_meta / minute_klines / minute_kline_meta
//! - **News**: news_items / article_contents (+ news_fts)
//! - **系统**: schema_meta / app_state (KV) / scheduler_heartbeat

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

    // 首次启用 FTS5 后 backfill 历史数据。
    // 触发器只会同步 *之后* 的写入；老用户启动时 news_items 已有数据但 news_fts 是空的，
    // 必须显式 backfill 一次。检测方式：news_fts 行数 = 0 且 news_items 行数 > 0。
    backfill_news_fts_if_needed(connection)?;
    Ok(())
}

/// 把现存 news_items 的 title/summary/source 灌进 news_fts。
/// 仅在 news_fts 空且 news_items 非空时执行（启用 FTS5 后第一次启动）。
fn backfill_news_fts_if_needed(connection: &Connection) -> Result<(), String> {
    let fts_count: i64 = connection
        .query_row("select count(*) from news_fts", [], |r| r.get(0))
        .unwrap_or(0);
    let items_count: i64 = connection
        .query_row("select count(*) from news_items", [], |r| r.get(0))
        .unwrap_or(0);
    if fts_count > 0 || items_count == 0 {
        return Ok(());
    }
    tracing::info!(items_count, "首次启用 news FTS5，开始 backfill 历史索引");
    connection
        .execute(
            "insert into news_fts (news_id, title, summary, source)
             select id,
                    coalesce(json_extract(payload_json, '$.title'), ''),
                    coalesce(json_extract(payload_json, '$.summary'), ''),
                    source
             from news_items",
            [],
        )
        .map_err(|err| format!("news_fts backfill 失败：{err}"))?;
    tracing::info!(items_count, "news FTS5 backfill 完成");
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
-- News 模块只提供"拉取 + 存储 + 查询"——任何分析 / 消费状态 / 调度都属于 Agent BC，
-- 见 agent_news_analysis_state 表与 pipeline/agent/news_batch_loop。
create table if not exists news_items (
    id text primary key,
    source text not null,
    published text,
    payload_json text not null,
    created_at text not null,
    updated_at text not null
);
create index if not exists idx_news_items_published
    on news_items(published desc);

create table if not exists article_contents (
    url text primary key,
    item_id text,
    payload_json text not null,
    fetched_at text not null
);

-- ===== Account BC =====
-- v4：Position 一肩挑执行 + 假设——payload_json 内嵌 kind/direction/signals_used/
-- invalidation_signals/reasoning/take_profit/stop_loss/time_stop_at 等字段。
-- 表只保留行级索引列，详细字段全在 payload_json。
create table if not exists simulated_positions (
    id text primary key,
    code text not null,
    source_analysis_id text not null,
    status text not null,
    kind text not null default 'live',    -- live / watch
    payload_json text not null,
    created_at text not null,
    updated_at text not null
);
create index if not exists idx_simulated_positions_code_status on simulated_positions(code, status);
create index if not exists idx_simulated_positions_status_kind on simulated_positions(status, kind);

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

-- 失效信号审计（spec account-module.md §4「调用时必须先写 invalidation_signal_recorded 事件。
-- 无论保护条件是否启用都要记录该事件」）
create table if not exists account_signal_audit (
    id text primary key,
    position_id text not null,
    signal text not null,
    reason text,
    hit integer not null default 0,
    occurred_at text not null
);
create index if not exists idx_account_signal_audit_pos on account_signal_audit(position_id, occurred_at desc);

-- Orders（spec account-module.md §2 Order）—— canonical 委托模型最小持久化。
-- 完整 6 态 OrderStatus + intent + actor，便于 operate_account 的所有 7 action 落库。
create table if not exists account_orders (
    order_id text primary key,
    ts_code text not null,
    side text not null check (side in ('buy','sell')),
    order_type text not null check (order_type in ('market','limit')),
    limit_price real,
    quantity integer not null,
    filled_quantity integer not null default 0,
    status text not null check (status in
        ('pending','partially_filled','filled','cancelled','rejected','expired')),
    intent text not null check (intent in
        ('open_position','scale_in','scale_out','close_position','direct_order')),
    position_id text,
    reason text,
    actor text not null check (actor in ('agent','system','user')),
    created_at text not null,
    updated_at text not null,
    expires_at text
);
create index if not exists idx_account_orders_status on account_orders(status, updated_at desc);
create index if not exists idx_account_orders_ts on account_orders(ts_code, status);
create index if not exists idx_account_orders_position on account_orders(position_id);

-- WatchlistEvent（spec account-module.md §2 watchlist_added/removed/note_updated）
-- 当前阶段独立于 position_events，最小可重建集：actor + ts_code + 可选 note
create table if not exists watchlist_events (
    event_id text primary key,
    event_type text not null check (event_type in
        ('watchlist_added','watchlist_removed','watchlist_note_updated')),
    actor text not null check (actor in ('agent','system','user')),
    ts_code text not null,
    note text,
    reason text,
    occurred_at text not null
);
create index if not exists idx_watchlist_events_ts on watchlist_events(ts_code, occurred_at desc);
create index if not exists idx_watchlist_events_occurred on watchlist_events(occurred_at desc);

create table if not exists watchlist_notes (
    ts_code text primary key,
    note text not null,
    updated_at text not null
);

-- AccountTrigger（spec account-module.md §2）
create table if not exists account_triggers (
    trigger_id text primary key,
    trigger_type text not null check (trigger_type in
        ('stop_loss','take_profit','time_stop','order_filled','order_rejected','order_expired','invalidated')),
    order_id text,
    position_id text,
    ts_code text,
    price real,
    threshold_json text,
    quote_freshness_json text,
    warnings_json text,
    event_id text not null,
    handled integer not null default 0,
    occurred_at text not null,
    created_at text not null,
    updated_at text not null
);
create index if not exists idx_account_triggers_handled on account_triggers(handled, occurred_at);
create index if not exists idx_account_triggers_position on account_triggers(position_id);

-- AccountEvent（spec account-module.md §2 账户事件模型）—— 统一的 21 类型 append-only
-- 真源。订单 / 成交 / 仓位 / lot / 保护条件 / 自选 / cash 冻结释放 / 触发器都先写到
-- 这里再更新派生读模型。`PositionEvent` 仍保留为现有 snapshot 派生路径；两者并存
-- 直到 snapshot 完全迁移到该流。
create table if not exists account_events (
    event_id text primary key,
    event_type text not null check (event_type in (
        'account_initialized', 'order_placed', 'order_cancelled', 'order_rejected',
        'order_expired', 'order_partially_filled', 'order_filled',
        'position_opened', 'position_scaled', 'position_closed', 'protection_adjusted',
        'watchlist_added', 'watchlist_removed', 'watchlist_note_updated',
        'cash_frozen', 'cash_released', 'shares_frozen', 'shares_released',
        'invalidation_signal_recorded', 'trigger_created', 'trigger_handled', 'snapshot_rebuilt'
    )),
    order_id text,
    fill_id text,
    position_id text,
    ts_code text,
    reason text,
    actor text not null check (actor in ('agent', 'system', 'user')),
    payload_json text not null,
    occurred_at text not null
);
create index if not exists idx_account_events_occurred on account_events(occurred_at desc);
create index if not exists idx_account_events_type on account_events(event_type, occurred_at desc);
create index if not exists idx_account_events_order on account_events(order_id) where order_id is not null;
create index if not exists idx_account_events_position on account_events(position_id) where position_id is not null;

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
    position_ids text,                    -- v4：JSON array of PositionId（agent run 内 open/close 涉及的 positions）
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

-- AgentTooLCall —— spec agent-infra-module.md §2。
-- 每个 local + server-side tool 调用一行；操作 Account / 写副作用的 tool 必须
-- 通过 inputPayloadRef / outputPayloadRef 持久化完整结构化 payload。
create table if not exists agent_tool_calls (
    tool_call_id text primary key,
    run_id text not null,
    name text not null,
    source text not null check (source in ('local_tool', 'server_side_tool')),
    input_summary_json text,
    output_summary_json text,
    input_payload_ref text,
    output_payload_ref text,
    -- spec agent-infra-module.md §2 + agent-runtime-module.md §2:
    -- 写副作用工具（operate_account）必须落结构化 payload；recovery 不允许依赖
    -- output_summary_json 解析。本列承载持久化后的结构化 Account result snapshot。
    output_payload_json text,
    is_error integer not null default 0,
    error_code text,
    started_at text not null,
    ended_at text,
    duration_ms integer
);
create index if not exists idx_agent_tool_calls_run on agent_tool_calls(run_id, started_at);

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

-- ===== Agent Runtime BC =====
-- 对齐 docs/design/agent-runtime-module.md。一次性建表；旧的 heuristic / lesson /
-- expectation 表已随 spec 重构移除，老 DB 走 SCHEMA_VERSION rename 路径。

-- AgentRun：一次产品语义上的 Agent 运行
create table if not exists agent_runs (
    run_id text primary key,
    profile_id text not null check (profile_id in
        ('user_chat','news_analysis','account_trigger_response','scheduled_review','manual_replay')),
    trigger_kind text not null check (trigger_kind in
        ('user_chat','news_batch','account_trigger','scheduled_review','manual_replay')),
    trigger_payload_json text not null,
    provider text not null,
    wire_format text not null,
    model text not null,
    status text not null check (status in ('queued','running','completed','failed','cancelled')),
    started_at text,
    ended_at text,
    error text,
    created_at text not null,
    updated_at text not null
);
create index if not exists idx_agent_runs_status on agent_runs(status, created_at desc);
create index if not exists idx_agent_runs_profile on agent_runs(profile_id, created_at desc);

-- DecisionEpisode：可复盘的最小投资判断单元
create table if not exists decision_episodes (
    episode_id text primary key,
    run_id text not null,
    trigger_kind text not null,
    symbols_json text not null,
    thesis text not null,
    action text not null check (action in
        ('no_action','add_watchlist','remove_watchlist','place_order','cancel_order',
         'open_position','scale_position','close_position','adjust_protection','record_invalidation_signal')),
    action_status text not null check (action_status in
        ('no_action','intended','submitted','blocked','deferred')),
    blocked_reason text,
    confidence real,
    risk_plan_json text,
    strategy_ids_json text not null,
    evidence_refs_json text not null,
    created_at text not null,
    updated_at text not null default ''
);
create index if not exists idx_decision_episodes_run on decision_episodes(run_id);
create index if not exists idx_decision_episodes_action on decision_episodes(action, action_status);
create index if not exists idx_decision_episodes_created on decision_episodes(created_at desc);

-- TradeIntent：operate_account 写工具的持久化意图 / 审计
create table if not exists trade_intents (
    intent_id text primary key,
    run_id text not null,
    episode_id text not null,
    tool_call_id text,
    account_input_json text not null,
    reason text not null,
    strategy_ids_json text not null,
    status text not null check (status in ('proposed','submitted','accepted','rejected','executed')),
    account_result_ref_json text,
    created_at text not null,
    updated_at text not null
);
create index if not exists idx_trade_intents_run on trade_intents(run_id);
create index if not exists idx_trade_intents_episode on trade_intents(episode_id);
create index if not exists idx_trade_intents_status on trade_intents(status, updated_at desc);

-- Account 反查索引：order_id -> intent_id / episode_id / run_id
create table if not exists agent_order_intent_index (
    order_id text primary key,
    intent_id text not null,
    episode_id text not null,
    run_id text not null,
    tool_call_id text,
    created_at text not null
);
create index if not exists idx_aoii_intent on agent_order_intent_index(intent_id);

-- DecisionReview：复盘记录
create table if not exists decision_reviews (
    review_id text primary key,
    episode_id text not null,
    trigger text not null check (trigger in
        ('position_closed','stop_loss','take_profit','time_stop','invalidated',
         'order_filled','order_rejected','order_expired','scheduled_review','manual_review')),
    result_json text,
    conclusion text not null,
    suggested_change_json text,
    evidence_refs_json text not null,
    warnings_json text,
    created_at text not null
);
create index if not exists idx_decision_reviews_episode on decision_reviews(episode_id);
create index if not exists idx_decision_reviews_created on decision_reviews(created_at desc);

-- StrategyCard：注入 Agent 上下文的策略卡
create table if not exists strategy_cards (
    strategy_id text primary key,
    version integer not null default 1,
    name text not null,
    description text not null,
    status text not null check (status in ('active','paused')),
    config_json text not null,
    created_at text not null,
    updated_at text not null
);
create index if not exists idx_strategy_cards_status on strategy_cards(status);

-- StrategyCard 历史审计：spec §2「每次策略卡调整必须递增 version，并保留旧版本可追溯」
create table if not exists strategy_card_audit (
    strategy_id text not null,
    version integer not null,
    name text not null,
    description text not null,
    status text not null check (status in ('active','paused')),
    config_json text not null,
    reason text not null,
    recorded_at text not null,
    primary key (strategy_id, version)
);
create index if not exists idx_strategy_card_audit_strategy on strategy_card_audit(strategy_id, version desc);

-- AgentRuntimeEventConsumption：跨模块事件消费幂等记录
-- (event_type, event_key, consumer) 是 unique
create table if not exists agent_event_consumption (
    event_type text not null,
    event_key text not null,
    consumer text not null,
    status text not null check (status in ('processing','consumed','ignored','failed')),
    run_id text,
    error text,
    created_at text not null,
    updated_at text not null,
    primary key (event_type, event_key, consumer)
);
create index if not exists idx_aec_status on agent_event_consumption(status, updated_at);

-- AgentNewsBufferItem：待分析新闻 buffer（durable）
create table if not exists agent_news_buffer (
    news_id text primary key,
    source_batch_id text not null,
    status text not null check (status in
        ('pending','in_batch','consumed','failed','ignored')),
    run_id text,
    entered_at text not null,
    updated_at text not null,
    retry_count integer not null default 0,
    next_retry_at text,
    last_error text
);
create index if not exists idx_anb_status_entered on agent_news_buffer(status, entered_at);
create index if not exists idx_anb_status_retry on agent_news_buffer(status, next_retry_at);

-- ===== News FTS5 全文索引 =====
--
-- 用 trigram tokenizer——SQLite 3.34+ 自带，对中文友好：把文本切成 3 字符
-- 窗口去索引，"光模块" 这种三字短语能精确命中而不用分词。
-- 单字 / 双字查询会降级到全表扫，但数据集 30 天 ≈ 1-2 万条，扫描仍快。
--
-- 用 contentless（无 content= 子句）：自管副本，无需外键。news_id 保留原文
-- id 用于回查 news_items；title/summary/source 是索引列。
create virtual table if not exists news_fts using fts5(
    news_id UNINDEXED,
    title,
    summary,
    source,
    tokenize = 'trigram'
);

-- 自动同步触发器——news_items 写入 / 修改 / 删除时联动 news_fts。
-- backfill 见 migrate() 里的 backfill_news_fts_if_needed（首次启用时一次性灌历史）。
create trigger if not exists news_items_ai_fts after insert on news_items begin
    insert into news_fts (news_id, title, summary, source) values (
        new.id,
        coalesce(json_extract(new.payload_json, '$.title'), ''),
        coalesce(json_extract(new.payload_json, '$.summary'), ''),
        new.source
    );
end;

create trigger if not exists news_items_au_fts after update of payload_json on news_items begin
    delete from news_fts where news_id = old.id;
    insert into news_fts (news_id, title, summary, source) values (
        new.id,
        coalesce(json_extract(new.payload_json, '$.title'), ''),
        coalesce(json_extract(new.payload_json, '$.summary'), ''),
        new.source
    );
end;

create trigger if not exists news_items_ad_fts after delete on news_items begin
    delete from news_fts where news_id = old.id;
end;

-- ===== 调度器心跳 + 审计 =====
-- 每个后台 loop 一行；每次 tick 完成（成功或失败）upsert 一次。
-- 前端可以查 "X loop 多久没成功了" → 决定是否告警。
create table if not exists scheduler_heartbeat (
    loop_name text primary key,
    last_ok_at text,
    last_err_at text,
    last_err_msg text,
    consecutive_err integer not null default 0,
    updated_at text not null
);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn open_in_memory_with_schema() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    fn insert_news(conn: &Connection, id: &str, source: &str, title: &str, summary: &str) {
        let payload = serde_json::json!({
            "id": id,
            "source": source,
            "title": title,
            "summary": summary,
            "link": "https://example.com",
            "published": "2025-01-01T00:00:00Z",
            "status": "consumed"
        })
        .to_string();
        conn.execute(
            "insert into news_items (id, source, published, payload_json, created_at, updated_at)
             values (?1, ?2, ?3, ?4, ?5, ?5)",
            params![id, source, "2025-01-01T00:00:00Z", payload, now()],
        )
        .unwrap();
    }

    #[test]
    fn fts_trigger_syncs_on_insert() {
        let conn = open_in_memory_with_schema();
        insert_news(&conn, "n1", "cls", "光模块板块异动", "AI 需求驱动光模块涨停潮");
        let count: i64 = conn
            .query_row("select count(*) from news_fts where news_id = 'n1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "插入 news_items 后触发器应该自动写入 news_fts");
    }

    #[test]
    fn fts_like_chinese_short_and_long() {
        // trigram MATCH 要求 3+ 字符，但 trigram 也加速 LIKE——任意长度都能用。
        // 这是 search_news_items_fts 选择 LIKE 而非 MATCH 的根本原因。
        let conn = open_in_memory_with_schema();
        insert_news(&conn, "n1", "cls", "光模块板块异动", "AI 需求驱动光模块涨停潮");
        insert_news(&conn, "n2", "jin10", "央行降准 0.5 个百分点", "释放长期资金");
        insert_news(&conn, "n3", "wallstreetcn", "白酒板块走弱", "茅台五粮液跌幅居前");

        // 3+ 字符短语
        let hits_3char: Vec<String> = conn
            .prepare("select news_id from news_fts where title like '%光模块%'")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(hits_3char, vec!["n1".to_string()]);

        // 2 字符短语（MATCH 会 0 命中，LIKE 借 trigram 加速仍精确）
        let hits_2char: Vec<String> = conn
            .prepare("select news_id from news_fts where title like '%央行%'")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(hits_2char, vec!["n2".to_string()], "2 字短语必须能匹配——这是中文常态");
    }

    #[test]
    fn fts_trigger_syncs_on_delete() {
        let conn = open_in_memory_with_schema();
        insert_news(&conn, "n1", "cls", "光模块板块异动", "涨停潮");
        conn.execute("delete from news_items where id = 'n1'", []).unwrap();
        let count: i64 = conn
            .query_row("select count(*) from news_fts where news_id = 'n1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "删除 news_items 后触发器应该清掉 news_fts 对应行");
    }

    #[test]
    fn backfill_populates_fts_from_existing_items() {
        // 模拟"FTS5 启用前已有数据"：先开 conn 手动建 news_items（含新列），写若干条，
        // 然后跑 migrate 让 backfill 跑一遍。
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "create table news_items (
                id text primary key,
                source text not null,
                published text,
                payload_json text not null,
                created_at text not null,
                updated_at text not null
            );",
        )
        .unwrap();
        for (id, title) in [("n1", "光模块涨停"), ("n2", "央行降准")] {
            let payload = serde_json::json!({"id": id, "title": title, "summary": ""}).to_string();
            conn.execute(
                "insert into news_items (id, source, published, payload_json, created_at, updated_at)
                 values (?1, 'cls', '2025-01-01', ?2, '2025', '2025')",
                params![id, payload],
            )
            .unwrap();
        }
        // 现在跑 migrate——会建 news_fts + 触发器 + 调 backfill
        migrate(&conn).unwrap();
        let count: i64 = conn
            .query_row("select count(*) from news_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2, "backfill 应该把老数据灌进 news_fts");
        // 验证 backfill 数据能被 FTS5 查到
        let hit: i64 = conn
            .query_row(
                "select count(*) from news_fts where news_fts match '\"光模块\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hit, 1);
    }
}
