#![allow(dead_code, unused_imports)] // 仓位 repo / valuation / migration 提供完整能力面，agent 写工具 Step B 连通

//! Infrastructure `account`——模拟账户子域的 I/O 实现。
//!
//! 包含：
//! - `repository`：positions + events 的 SQLite 读写（含 domain ↔ DB 投射）
//! - `watchlist`：Account-owned 自选股 KV 持久化；Quotes 只通过 subscriptions 消费
//! - `valuation`：从 events + MARKET_SNAPSHOT 派生 AccountSnapshot
//! - `migration`：legacy positions（无 events 的老数据）反向生成 opened/closed events
//!
//! 依赖单向：account → quotes（valuation 读 MARKET_SNAPSHOT），spec § 1.3 允许。

pub mod migration;
pub mod repository;
pub mod snapshot_cache;
pub mod valuation;
pub mod watchlist;

pub use repository::PositionRepo;
pub use valuation::{compute_snapshot, INITIAL_CASH};
