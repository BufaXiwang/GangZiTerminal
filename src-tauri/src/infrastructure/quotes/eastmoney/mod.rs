//! Eastmoney HTTP adapter——实时报价 + 历史 K 线 + 分时图。
//!
//! - `client`：HTTP client + retry
//! - `realtime`：ulist.np 实时报价（含五档盘口 + 内外盘）
//! - `kline`：push2his kline klt=1/5/15/30/60
//! - `minutes`：push2his trends2 分时图

pub mod client;
pub mod kline;
pub mod realtime;
