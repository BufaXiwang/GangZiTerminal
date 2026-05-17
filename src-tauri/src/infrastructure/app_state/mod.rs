//! Infrastructure `app_state`——SQLite `app_state` KV 表的通用读写。
//!
//! 跨 BC 共用的 K-V 存储——agent.config / watchlist / tushare-token / proxy-pool /
//! kline-cache 等"业务状态"都借用这张表。每个 key 由调用 BC 自己定义命名空间。
//!
//! Tauri IPC（前端读/写）在 `adapters::app_state_commands`。

pub mod repository;

pub use repository::{delete_app_state_value, load_app_state_value, save_app_state_value};
