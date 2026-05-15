//! 缓存层——SQLite 持久化的"读 / 增量补全"封装。
//!
//! - `kline_cache`：日/周/月 K 增量缓存（10 min TTL）
//! - `minute_kline_cache`：分钟 K（1m/5m/15m/30m/60m）短 TTL 缓存（盘中 30s / 盘外 5min）

pub mod kline_cache;
pub mod minute_kline_cache;
