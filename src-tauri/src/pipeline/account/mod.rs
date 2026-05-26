#![allow(unused_imports)] // 各 use case re-export 提供完整 API surface

//! Pipeline `account`——模拟账户用例编排层。
//!
//! 由 IPC adapter / scheduler / agent tool 调用，**唯一的写入口**。
//!
//! 四个子模块：
//! - `service`：`AccountService`（含 Mutex 写锁）—— 5 个写操作 + snapshot 读
//! - `canonical`：spec `operate_account` 7 action dispatch shim（UI + Agent 共用）
//! - `update_watchlist`：spec `update_watchlist` 非交易写
//! - `subscriptions`：暴露 `subscribed_codes()` 给 quotes refresh 用

pub mod canonical;
pub mod service;
pub mod subscriptions;
pub mod update_watchlist;

pub use service::{AccountService, OpenRequest};
pub use subscriptions::subscribed_codes;
pub use update_watchlist::{
    dispatch as update_watchlist_dispatch, parse_action as parse_watchlist_action,
    UpdateWatchlistInput, UpdateWatchlistResponse, WatchlistAction,
};
