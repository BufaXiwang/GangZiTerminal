//! 市场数据 use case 集合——行情刷新 / 大盘拼装 / universe 维护 / K 线预热。
//!
//! - `refresh`：盘中 tick 拉实时报价喂 dispatch（订阅 + core indexes）
//! - `overview`：大盘四大指数 + breadth 拼装
//! - `universe`：A 股 stock universe 刷新（停牌 / 退市 / 新股入池）
//! - `kline_warm`：watchlist + open positions 的日 K 预热

pub mod kline_warm;
pub mod overview;
pub mod refresh;
pub mod universe;
