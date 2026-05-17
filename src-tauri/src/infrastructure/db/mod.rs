//! Infrastructure `db`——SQLite 客户端 + schema migration。
//!
//! 子模块：
//! - `connection`: open_database / database_path / DatabaseInfo / SCHEMA_VERSION
//! - `migrations`: migrate（schema 单一来源）+ 升级 helpers
//! - `helpers`: now / JSON 通用工具

pub mod connection;
pub mod helpers;
pub mod migrations;

pub use connection::{database_path, open_database, DatabaseInfo, SCHEMA_VERSION};
pub use helpers::{json_string, list_json_payloads, now, required_json_string};
pub use migrations::migrate;
