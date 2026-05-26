//! Migration runner — 集中执行所有 BC 的 schema 迁移。
//!
//! Spec: AGENTS.md（schema 迁移：rusqlite_migration，一个改动一个 .sql，启动自动 apply）
//!
//! 各 BC 在自己的 `infrastructure/<bc>/migrations.rs` 中导出 `migrations() -> Vec<M<'static>>`，
//! 本 runner 在启动时按 BC 顺序合并并 apply。
//!
//! 当前阶段只暴露 runner 接口，不内含具体 migration——
//! Phase 1 / Phase 2 sub-agent 在各自 BC 的 infrastructure 里加 migration 并接入。

use rusqlite::Connection;
use rusqlite_migration::{Migrations, M};

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
