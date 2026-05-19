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
    -- v5 审计字段：追溯到生成这条预期的 scan tick + 主信号家族（用于快速 group by）
    trigger_signal_family text,
    source_episode_id text,
    created_at text not null,
    expires_at text not null,
    closed_at text
);
create index if not exists idx_expectations_code_state on expectations(code, state);
create index if not exists idx_expectations_state_expires on expectations(state, expires_at);
create index if not exists idx_expectations_theme on expectations(theme);
create index if not exists idx_expectations_source_episode on expectations(source_episode_id);

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
    source_episode_id text,                -- v5：追溯到产生此 lesson 的 reflection episode
    created_at text not null
);
create index if not exists idx_lessons_expectation on lessons(expectation_id);
create index if not exists idx_lessons_code_time on lessons(code, created_at desc);
create index if not exists idx_lessons_empty_takeaway on lessons(created_at desc) where takeaway = '';

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
    last_emerged_at text,                  -- v5：最后一次被 emerge 流程"新生成"的时间——前端用来识别"本周新增"
    retired_at text,
    retired_reason text,
    created_at text not null
);
create index if not exists idx_heuristics_origin on heuristics(origin);
create index if not exists idx_heuristics_retired on heuristics(retired_at);
create index if not exists idx_heuristics_emerged on heuristics(last_emerged_at desc);

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

-- ===== News FTS5 全文索引（v5.1 in-place add，不 bump schema version） =====
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

-- ===== v5：调度器心跳 + 审计 =====
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
        // 模拟"FTS5 启用前已有数据"：先开 conn 关掉触发器 + 表，写入若干 news_items，
        // 然后重新 migrate 让 backfill 跑一遍。
        let conn = Connection::open_in_memory().unwrap();
        // 先建 news_items 表（手动，不带 fts），写两条
        conn.execute_batch(
            "create table news_items (
                id text primary key, source text not null, published text,
                payload_json text not null, created_at text not null, updated_at text not null
            );",
        )
        .unwrap();
        for (id, title) in [("n1", "光模块涨停"), ("n2", "央行降准")] {
            let payload = serde_json::json!({"id": id, "title": title, "summary": ""}).to_string();
            conn.execute(
                "insert into news_items values (?1, 'cls', '2025-01-01', ?2, '2025', '2025')",
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
