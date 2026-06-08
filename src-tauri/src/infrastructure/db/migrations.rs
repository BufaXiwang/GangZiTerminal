//! Migration runner + 全局 migration 列表。
//!
//! Spec: AGENTS.md（schema 迁移：rusqlite_migration，一个改动一个 .sql，启动自动 apply）
//!
//! `all_migrations()` 是全局 migration 真源（唯一拼接点）。`run_migrations()` 接受任意
//! `Vec<M>` 执行——既供全局使用，也供各 BC 测试用自己的子集初始化 in-memory DB。
//!
//! ## 全局序号（append-only，不可重排）
//!
//! `rusqlite_migration` 用 `user_version`（0-indexed 位置）跟踪已 apply 的最高 migration。
//! 任何重排/插入都会让存量 DB 的 `user_version` 指向错误的 migration → CREATE TABLE 重跑 → panic。
//! **新增 migration 只能追加到 `all_migrations()` 尾部。**

use rusqlite::Connection;
use rusqlite_migration::{Migrations, M};

/// 全局 migration 列表（唯一真源）。
///
/// 把所有 BC 的 migration 条目按**固定全局序号**原样排列。序号 = 数组下标（0-indexed），
/// 与 `rusqlite_migration` 的 `user_version` 一一对应。**append-only：新增只能加末尾。**
///
/// 当前固定顺序（不要重排）：
///
/// | 序号 | BC        | 说明                                     |
/// |------|-----------|------------------------------------------|
/// |  0   | News      | 001 初始表（news_items / articles / sources / fts） |
/// |  1   | Account   | 001 初始表（meta / events / orders / fills / positions / lots / protections / watchlist / triggers / freezes / event_seq） |
/// |  2   | Account   | 002 operation_dedup 表                    |
/// |  3   | Account   | 003 account_orders 增 client_order_id 列  |
/// |  4   | Quotes    | 001 初始表（instruments / klines / intraday / daily_basic / company_events / close_snapshot / trade_calendar / refresh_state） |
/// |  5   | Quotes    | 002 xdxr_events 除权除息表                |
/// |  6   | Agent     | 001 初始表（messages / tool_calls / provider_channels / payloads） |
/// |  7   | Agent     | 002 channel auth/active 列                |
/// |  8   | Agent     | 003 messages conversation 列              |
/// |  9   | Agent     | 004 Runtime 真源表（runs / strategy / analysis / trades / order_run_index / news_buffer / event_consumption / heartbeats） |
/// | 10   | Agent     | 005 settings kv 表                        |
/// | 11   | Agent     | 006 review_suggestions + daily_equity（deprecated） |
/// | 12   | Account   | tail-001 account_day_equity（日初权益基线 + 高水位） |
/// | 13   | Account   | tail-002 account_archive（账户重置归档摘要） |
pub fn all_migrations() -> Vec<M<'static>> {
    use crate::infrastructure::account::migrations as account_mig;
    use crate::infrastructure::agent::migrations as agent_mig;
    use crate::infrastructure::news::migrations as news_mig;
    use crate::infrastructure::quotes::migrations as quotes_mig;

    let mut all = Vec::new();
    // Spec: architecture.md — 全局拼接顺序 news → account → quotes → agent → account_tail
    all.extend(news_mig::migrations());      // [0]       News 001
    all.extend(account_mig::migrations());   // [1..3]    Account 001–003
    all.extend(quotes_mig::migrations());    // [4..5]    Quotes 001–002
    all.extend(agent_mig::migrations());     // [6..11]   Agent 001–006
    all.extend(account_mig::migrations_tail()); // [12..13]  Account tail-001–002
    all
}

/// Apply 所有 BC 的迁移。
///
/// 空集合直接返回 Ok——Phase 0 阶段尚无 BC migration 注册。
pub fn run_migrations(conn: &mut Connection, all: Vec<M<'static>>) -> rusqlite_migration::Result<()> {
    if all.is_empty() {
        return Ok(());
    }
    let migrations = Migrations::new(all);
    migrations.to_latest(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn runs_empty_migration_set() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn, vec![]).unwrap();
    }

    #[test]
    fn applies_a_trivial_migration() {
        let mut conn = Connection::open_in_memory().unwrap();
        let migs = vec![M::up("CREATE TABLE smoke (id INTEGER PRIMARY KEY);")];
        run_migrations(&mut conn, migs).unwrap();
        conn.execute("INSERT INTO smoke (id) VALUES (1)", []).unwrap();
    }
}
