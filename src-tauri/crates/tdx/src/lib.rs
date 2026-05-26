//! 通达信 (TDX) 行情协议 Rust 端口——复刻 mootdx / pytdx wire format。
//!
//! **参考实现**：
//! - Python 版上游：<https://github.com/mootdx/mootdx>
//! - Rust 端口参考：<https://github.com/mootdx/mootdx-rust>
//!
//! 模块组成：
//! - [`client`]：同步 TCP 客户端，连公共 HQ 服务器（[`hosts::HQ_HOSTS`]）拿实时行情 + K 线
//! - [`types`]：协议返回的原始数据结构（`Bar` / `SecurityQuote` / `SecurityListEntry` …）
//! - [`error`]：协议层错误类型（不依赖任何上层 domain）
//!
//! 优势 vs HTTP 接口（如 EM ulist.np）：
//! - 16 个分散的公共 HQ 服务器，单 IP 风控敏感度低
//! - 私有 TCP 二进制协议，反爬难
//! - 数据含五档盘口
//!
//! 限制：
//! - 只支持沪深两市（`Market` 枚举只有 SZ/SH），**北交所不支持**
//! - 同步阻塞 TCP，调用方需 `tokio::task::spawn_blocking` 包装
//! - 不支持除权，K 线序列除权日附近会有跳变

pub mod client;
pub mod error;
pub mod helper;
pub mod hosts;
pub mod types;

pub use client::TdxHqClient;
pub use error::{Error, Result};
pub use hosts::HQ_HOSTS;
pub use types::{Bar, BarCategory, Market, QuoteLevel, SecurityListEntry, SecurityQuote};
