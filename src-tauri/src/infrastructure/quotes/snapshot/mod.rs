//! 内存 snapshot——quotes 子域的当前状态 in-memory 维护。
//!
//! 设计（架构 § 1.5）：
//! - 读 API 同步、永远不阻塞——decouple agent decision path 和 I/O
//! - 写 API 由 fetcher / refresh loop 触发（market_quote_loop scheduler）
//!
//! 模块：
//! - `market_snapshot`：实时行情基础快照（ts_code key）—— **唯一**实时报价 snapshot
//!
//! 自选股属于 `infrastructure/account/watchlist.rs`；quotes refresh 只消费
//! `pipeline/account/subscriptions.rs` 暴露的订阅集合。
pub mod market_snapshot;
