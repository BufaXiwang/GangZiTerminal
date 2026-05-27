//! Quotes adapters — Tauri commands + 对外事件。
//!
//! Spec: docs/design/quotes-module.md §4 / §5
//!
//! 命令列表（lib.rs `collect_commands!` 引用）：
//! - `crate::adapters::quotes::cmd::list_market`
//! - `crate::adapters::quotes::cmd::fetch_data`
//! - `crate::adapters::quotes::cmd::scan_market`
//!
//! 事件常量：
//! - [`events::MARKET_QUOTES_REFRESHED_EVENT`] = `market-quotes-refreshed`

pub mod cmd;
pub mod events;
