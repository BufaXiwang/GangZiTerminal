#![allow(dead_code, unused_imports)] // TDX 协议层提供完整能力面，agent 工具按需调用；未连通的 API 保留供未来用

//! 通达信 (TDX) 行情协议 Rust 端口——完整复刻 mootdx / pytdx wire format。
//!
//! **参考实现**：
//! - Python 版上游：<https://github.com/mootdx/mootdx>
//! - Rust 端口参考：<https://github.com/mootdx/mootdx-rust>
//!
//! 两大功能：
//! - [`reader`]：离线解析本地 Tdx 数据文件（`.day` / `.lc1` / `.lc5`）—— 用户从通达信
//!   客户端下载完整历史的场景
//! - [`client`]：同步 TCP 客户端，连公共 HQ 服务器（[`hosts::HQ_HOSTS`]）拿实时行情
//!
//! 优势 vs HTTP 接口（如 EM ulist.np）：
//! - 16 个分散的公共 HQ 服务器，单 IP 风控敏感度低
//! - 私有 TCP 二进制协议，反爬难
//! - 数据含五档盘口
//!
//! 限制：
//! - 只支持沪深两市（Market 枚举只有 SZ/SH），**北交所不支持**
//! - 同步阻塞 TCP，调用方需 `tokio::task::spawn_blocking` 包装
//!
//! ## 接入到本项目
//!
//! 上层 [`crate::infrastructure::quotes::realtime::tdx`] 实现了 `RealtimeQuoteSource`
//! trait，把本模块的同步 client 包装成 async source 注入 dispatch 链。

pub mod client;
pub mod error;
pub mod helper;
pub mod hosts;
pub mod reader;
pub mod types;

pub use error::{Error, Result};
pub use types::{Bar, BarCategory, Market, QuoteLevel, SecurityListEntry, SecurityQuote};
