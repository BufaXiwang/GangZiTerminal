//! Infrastructure 层：I/O 实现。
//!
//! Spec: docs/design/architecture.md §2
//!
//! 铁律：infrastructure/ 不允许 use pipeline | adapters。

pub mod account;
pub mod agent;
pub mod db;
pub mod news;
pub mod quotes;
pub mod tracing;
