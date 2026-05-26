//! Domain 层：纯类型 + 规则。
//!
//! Spec: docs/design/architecture.md §2
//!
//! 铁律：domain/ 不允许 use tauri | rusqlite | reqwest | tdx | infrastructure | pipeline | adapters。

pub mod account;
pub mod agent;
pub mod news;
pub mod quotes;
pub mod shared;
