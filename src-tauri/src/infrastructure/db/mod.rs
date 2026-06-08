//! DB infrastructure：单 SQLite 文件 + migration runner。
//!
//! Spec: docs/design/architecture.md §2；AGENTS.md（持久化：单库 gangzi.db）

pub mod connection;
pub mod migrations;

pub use connection::AppDb;
pub use migrations::{all_migrations, run_migrations};
