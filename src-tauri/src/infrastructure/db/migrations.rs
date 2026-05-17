//! Schema 迁移 + 升级 helpers。
//!
//! `migrate()` 是 SQLite schema 的单一来源——所有 `CREATE TABLE IF NOT EXISTS` 在一个事务里跑。
//! 4 个 upgrade helper 处理跨版本的 ALTER（SQLite 不支持 ALTER CHECK，只能 rename → 新建 → copy → drop）。

use crate::infrastructure::db::connection::SCHEMA_VERSION;
use crate::infrastructure::db::helpers::now;
use rusqlite::{params, Connection};

pub fn migrate(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(
            "
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

            create table if not exists simulated_positions (
                id text primary key,
                code text not null,
                source_analysis_id text not null,
                status text not null,
                payload_json text not null,
                created_at text not null,
                updated_at text not null
            );

            create table if not exists agent_tasks (
                id text primary key,
                item_id text not null,
                title text not null,
                task_type text,
                agent_role text,
                status text not null check (status in ('queued', 'running', 'completed', 'failed', 'skipped')),
                priority integer not null default 100,
                attempts integer not null default 0,
                input_json text not null,
                result_json text,
                error text,
                session_id text,
                parent_task_id text,
                locked_at text,
                created_at text not null,
                updated_at text not null,
                started_at text,
                completed_at text
            );

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

            create table if not exists agent_runs (
                run_id text primary key,
                pipeline text not null check (pipeline in ('chat', 'briefing', 'review')),
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
                trigger_message_id text
            );
            create index if not exists idx_agent_runs_pipeline_started on agent_runs(pipeline, started_at desc);

            create table if not exists agent_run_turns (
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
            create index if not exists idx_agent_run_turns_run on agent_run_turns(run_id, turn);

            create table if not exists stocks (
                code text primary key,
                name text not null,
                sector text,
                market text not null,
                updated_at text not null
            );

            -- 大盘指数档案：ts_code (000001.SH 形式) 是 PK，和 stocks 不冲突
            create table if not exists indexes (
                ts_code text primary key,
                code text not null,
                name text not null,
                market text not null,        -- SSE / SZSE / CSI / SW（申万）
                publisher text,              -- 发布机构
                category text,               -- 大盘 / 行业 / 主题 / 风格
                updated_at text not null
            );

            -- 基金档案：ETF / LOF / 封基 等 ts_code 是 PK
            create table if not exists funds (
                ts_code text primary key,
                code text not null,
                name text not null,
                market text not null,        -- E (场内) / O (场外)
                fund_type text,              -- 股票型 / 混合型 / 债券型 / 货币型 / ETF / LOF
                management text,             -- 管理人
                list_date text,              -- 上市日 YYYYMMDD
                status text,                 -- L (上市) / D (退市)
                updated_at text not null
            );

            -- K 线缓存：个股 / 指数 / 基金 统一存这里
            -- ts_code 作为 PK 一部分，避免 000001 SH（上证）vs 000001 SZ（平安）冲突
            create table if not exists klines (
                ts_code text not null,       -- 形如 000001.SZ / 510300.SH / 399006.SZ
                period text not null,        -- day / week / month
                adjust text not null,        -- qfq / hfq / none
                date text not null,          -- YYYYMMDD
                open real not null,
                close real not null,
                high real not null,
                low real not null,
                volume real,
                amount real,
                source text not null,        -- tushare / em / stale
                primary key (ts_code, period, adjust, date)
            );

            create table if not exists kline_meta (
                ts_code text not null,
                period text not null,
                adjust text not null,
                last_known_date text not null,
                fetched_at text not null,
                primary key (ts_code, period, adjust)
            );

            -- 分钟 K 缓存：1m / 5m / 15m / 30m / 60m
            -- 走 EM push2his klt 端点拉取，盘中持续累加，TTL 30s
            create table if not exists minute_klines (
                ts_code text not null,
                period text not null,           -- 1m / 5m / 15m / 30m / 60m
                timestamp_ms integer not null,  -- unix ms（北京 9:30 = UTC 01:30）
                open real not null,
                close real not null,
                high real not null,
                low real not null,
                volume integer not null,
                amount real not null,
                source text not null,
                primary key (ts_code, period, timestamp_ms)
            );

            create table if not exists minute_kline_meta (
                ts_code text not null,
                period text not null,
                last_known_ts integer not null,
                fetched_at text not null,
                primary key (ts_code, period)
            );

            create index if not exists idx_agent_tasks_status_priority on agent_tasks(status, priority, created_at);
            create index if not exists idx_simulated_positions_code_status on simulated_positions(code, status);
            create index if not exists idx_chat_messages_created on chat_messages(created_at desc);
            create index if not exists idx_chat_messages_kind on chat_messages(kind, created_at desc);
            create index if not exists idx_position_events_pos_time on position_events(position_id, occurred_at);
            create index if not exists idx_stocks_name on stocks(name);
            create index if not exists idx_indexes_name on indexes(name);
            create index if not exists idx_funds_name on funds(name);
            create index if not exists idx_funds_market on funds(market);
            -- 注意：klines 表的索引 idx_klines_ts_period_date 不在这里建——
            -- 必须等 upgrade_klines_to_ts_code 把旧 schema (列 `code`) 升级为新 schema (列 `ts_code`)
            -- 之后才能建。见 migrate() 末尾。
            ",
        )
        .map_err(|err| format!("初始化 SQLite schema 失败：{err}"))?;

    // K 线表 schema 升级：旧版列叫 `code`（6 位），新版叫 `ts_code`（带后缀）。
    // 老表存在且没有 ts_code 列 → DROP 重建（缓存数据丢就丢，反正是缓存）
    upgrade_klines_to_ts_code(connection)?;

    // upgrade 完成后，安全建 klines 的新索引（针对 ts_code 列）
    connection
        .execute_batch(
            "create index if not exists idx_minute_klines_ts_period_ts
             on minute_klines(ts_code, period, timestamp_ms desc);
             create index if not exists idx_klines_ts_period_date
             on klines(ts_code, period, adjust, date desc);",
        )
        .map_err(|err| format!("建 klines 索引失败：{err}"))?;

    // briefing/review 已下线——一次性清理：
    // 1. DROP analysis_records 表 + 其索引
    // 2. 重建 news_items 去掉 analysis_status 列（SQLite 不支持 DROP COLUMN）
    drop_briefing_review_remnants(connection)?;
    // 旧版多 session 对话表已弃用，直接清掉避免存量数据干扰
    connection
        .execute("drop table if exists chat_sessions", [])
        .map_err(|err| format!("清理旧 chat_sessions 表失败：{err}"))?;
    connection
        .execute("drop table if exists investor_memory_log", [])
        .map_err(|err| format!("清理旧 investor_memory_log 表失败：{err}"))?;
    // 旧行情 DB 快照已被 `MARKET_SNAPSHOT` in-memory 真源替代；不保留兼容。
    connection
        .execute("drop table if exists quote_snapshots", [])
        .map_err(|err| format!("清理旧 quote_snapshots 表失败：{err}"))?;
    connection
        .execute("drop table if exists market_quotes_snapshot", [])
        .map_err(|err| format!("清理旧 market_quotes_snapshot 表失败：{err}"))?;
    // codex 时代的 MCP 注册诊断状态——agent 直连后已不写入；老用户本地若残留
    // 一条失败记录会让前端 status bar 持续显示"MCP 工具未启用"，一次性清掉。
    connection
        .execute(
            "delete from app_state where key = 'gangzi-terminal.mcp-status'",
            [],
        )
        .map_err(|err| format!("清理旧 mcp-status key 失败：{err}"))?;
    // codex CLI 的长会话 id——agent 自管 messages，不再需要 resume；清掉避免误读。
    connection
        .execute(
            "delete from app_state where key = 'gangzi-terminal.main-agent-session-id'",
            [],
        )
        .map_err(|err| format!("清理旧 codex session-id key 失败：{err}"))?;
    add_column_if_missing(connection, "agent_tasks", "task_type", "text")?;
    add_column_if_missing(connection, "agent_tasks", "agent_role", "text")?;
    add_column_if_missing(
        connection,
        "agent_tasks",
        "attempts",
        "integer not null default 0",
    )?;
    add_column_if_missing(connection, "agent_tasks", "parent_task_id", "text")?;
    add_column_if_missing(connection, "agent_tasks", "locked_at", "text")?;

    // 老库的 chat_messages 表 CHECK 约束没有 'compact_boundary'——让 compact 边界
    // 行写入会失败。SQLite 不支持 ALTER CHECK，必须重建表。检查现有 sql 文本里
    // 是否已包含新值，否则做一次性重建。
    upgrade_chat_messages_kind_check(connection)?;

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

/// briefing / review 下线后的一次性清理（幂等）：
/// 1. DROP TABLE analysis_records + 其索引——表本身和索引都不再需要
/// 2. 重建 news_items 表去掉 analysis_status 列（SQLite 不支持 DROP COLUMN，
///    走 rename → 新建 → copy → drop legacy 的套路，事务里完成）
///
/// 两步都做"先看在不在再处理"的幂等判断，重启不会重做。
fn drop_briefing_review_remnants(connection: &Connection) -> Result<(), String> {
    // 1) 先扔掉 analysis_records 表 + 索引
    connection
        .execute_batch(
            "drop index if exists idx_analysis_records_item_id;
             drop table if exists analysis_records;",
        )
        .map_err(|err| format!("清理 analysis_records 失败：{err}"))?;

    // 2) news_items 表如果还含 analysis_status 列，重建去掉
    let has_status = {
        let mut stmt = connection
            .prepare("pragma table_info(news_items)")
            .map_err(|err| format!("读取 news_items schema 失败：{err}"))?;
        let cols: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|err| format!("读取 news_items schema 失败：{err}"))?
            .filter_map(|r| r.ok())
            .collect();
        cols.iter().any(|n| n == "analysis_status")
    };
    if has_status {
        tracing::info!("一次性清理：news_items 删除 analysis_status 列（重建表）");
        connection
            .execute_batch(
                "begin transaction;
                 drop index if exists idx_news_items_status_published;
                 alter table news_items rename to news_items_legacy;
                 create table news_items (
                    id text primary key,
                    source text not null,
                    published text,
                    payload_json text not null,
                    created_at text not null,
                    updated_at text not null
                 );
                 insert into news_items (id, source, published, payload_json, created_at, updated_at)
                 select id, source, published, payload_json, created_at, updated_at
                 from news_items_legacy;
                 drop table news_items_legacy;
                 commit;",
            )
            .map_err(|err| format!("重建 news_items（去掉 analysis_status）失败：{err}"))?;
    }
    Ok(())
}

/// 一次性升级 chat_messages 表的 kind CHECK 约束。
///
/// 触发条件：现有表的 CHECK 约束里仍包含 'briefing'（老 schema）。
/// 步骤：
/// 1. 先 DELETE 掉 kind='briefing'/'review' 的历史数据（briefing/review 已下线）
/// 2. 重建表去掉 'briefing'/'review' 允许值，并加入 'compact_boundary'
///
/// SQLite 不支持 ALTER CHECK，只能 rename → 新建 → copy → drop。整套包事务。
fn upgrade_chat_messages_kind_check(connection: &Connection) -> Result<(), String> {
    let existing_sql: Option<String> = connection
        .query_row(
            "select sql from sqlite_master where type='table' and name='chat_messages'",
            [],
            |row| row.get(0),
        )
        .ok();
    let needs_upgrade = match existing_sql {
        // 老 schema 含 'briefing' 字面量 → 重建；新 schema 已经没有这串了
        Some(sql) => sql.contains("'briefing'"),
        None => false, // 表不存在——上一步 create table if not exists 已用新约束建好
    };
    if !needs_upgrade {
        return Ok(());
    }
    tracing::info!("升级 chat_messages.kind CHECK：去掉 briefing/review，加 compact_boundary");
    connection
        .execute_batch(
            "begin transaction;
             delete from chat_messages where kind in ('briefing', 'review');
             alter table chat_messages rename to chat_messages_legacy;
             create table chat_messages (
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
             insert into chat_messages
                (id, created_at, role, kind, content_md, content_json,
                 source_task_id, source_news_ids, source_record_id)
             select id, created_at, role, kind, content_md, content_json,
                    source_task_id, source_news_ids, source_record_id
             from chat_messages_legacy;
             drop table chat_messages_legacy;
             create index if not exists idx_chat_messages_created on chat_messages(created_at desc);
             create index if not exists idx_chat_messages_kind on chat_messages(kind, created_at desc);
             commit;",
        )
        .map_err(|err| format!("升级 chat_messages CHECK 约束失败：{err}"))?;
    Ok(())
}

/// K 线表 schema 升级——把旧 `klines`/`kline_meta`（列 `code` 6 位）替换为新版（列 `ts_code` 带后缀）。
/// 旧 schema 不能容纳指数/基金（000001.SH 和 000001.SZ 冲突），直接 DROP 重建。缓存丢就丢。
fn upgrade_klines_to_ts_code(connection: &Connection) -> Result<(), String> {
    let has_ts_code = |table: &str| -> Result<bool, String> {
        let mut stmt = connection
            .prepare(&format!("pragma table_info({table})"))
            .map_err(|err| format!("读取 {table} schema 失败：{err}"))?;
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|err| format!("读取 {table} schema 失败：{err}"))?
            .filter_map(|r| r.ok())
            .collect();
        if names.is_empty() {
            return Ok(true); // 表不存在 → 由 create table if not exists 建好
        }
        Ok(names.iter().any(|n| n == "ts_code"))
    };

    if !has_ts_code("klines")? {
        tracing::info!("升级 klines schema：DROP + 重建（旧缓存丢失，下次访问会重拉）");
        connection
            .execute_batch(
                "drop table if exists klines;
                 create table klines (
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
                 create index idx_klines_ts_period_date on klines(ts_code, period, adjust, date desc);",
            )
            .map_err(|err| format!("升级 klines 失败：{err}"))?;
    }
    if !has_ts_code("kline_meta")? {
        tracing::info!("升级 kline_meta schema：DROP + 重建");
        connection
            .execute_batch(
                "drop table if exists kline_meta;
                 create table kline_meta (
                     ts_code text not null,
                     period text not null,
                     adjust text not null,
                     last_known_date text not null,
                     fetched_at text not null,
                     primary key (ts_code, period, adjust)
                 );",
            )
            .map_err(|err| format!("升级 kline_meta 失败：{err}"))?;
    }
    Ok(())
}

fn add_column_if_missing(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), String> {
    let mut statement = connection
        .prepare(&format!("pragma table_info({table})"))
        .map_err(|err| format!("读取 {table} schema 失败：{err}"))?;
    let exists = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|err| format!("读取 {table} schema 失败：{err}"))?
        .any(|name| name.map(|value| value == column).unwrap_or(false));
    if !exists {
        connection
            .execute(
                &format!("alter table {table} add column {column} {definition}"),
                [],
            )
            .map_err(|err| format!("升级 {table}.{column} 失败：{err}"))?;
    }
    Ok(())
}
