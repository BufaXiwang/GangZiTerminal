//! 单 SQLite 连接 wrapper。
//!
//! Spec: AGENTS.md（持久化：单库 gangzi.db）；docs/design/architecture.md §2
//!
//! 设计：
//! - 所有 BC 共享同一个 SQLite 文件 `gangzi.db`。
//! - 表前缀分区：`news_*` / `quote_*` / `account_*` / `agent_*`。
//! - 连接为 `Arc<Mutex<Connection>>`，pipeline / infrastructure 层共享一个实例。
//!   暂不引入连接池：SQLite 本身串行写，单连接 + Mutex 足够。
//!
//! 后续如证明需要并发读，再切换 `r2d2` / `deadpool-sqlite`。

use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// 应用 DB 句柄。Tauri State 持有一份。
#[derive(Clone)]
pub struct AppDb {
    inner: Arc<Mutex<Connection>>,
}

impl AppDb {
    /// 打开 / 创建 SQLite 文件并启用基础 pragma。
    pub fn open(path: &PathBuf) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)?;
        configure_pragmas(&conn)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
        })
    }

    /// 内存 DB，测试用。
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        configure_pragmas(&conn)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
        })
    }

    /// 借用底层连接执行一个闭包。
    pub fn with<R>(&self, f: impl FnOnce(&mut Connection) -> R) -> R {
        let mut guard = self.inner.lock().expect("AppDb mutex poisoned");
        f(&mut *guard)
    }
}

fn configure_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    // WAL 提升读并发；NORMAL 同步模式平衡安全与性能。
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_opens() {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| {
            let mode: String = c
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            // in-memory DB always reports 'memory' regardless of pragma; just assert open works.
            assert!(!mode.is_empty());
        });
    }
}
