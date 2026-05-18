#![allow(dead_code, unused_imports)] // newtype 提供完整 ctor / accessor 套件；Step B agent 工具按需用

//! Domain `shared`——跨 bounded context 复用的 newtype 与 value object。
//!
//! 这里的类型是**纯 domain**——无 I/O、无 Tauri、无外部 crate（除 serde / chrono / thiserror）。
//! 任何 crate::* 内部模块可以 `use crate::domain::shared::*` 引入。

pub mod ids;
pub mod money;
pub mod shares;
pub mod signal;
pub mod time;

pub use ids::{IdError, StockCode, TsCode};
pub use money::{KYuan, MoneyError, Yuan};
pub use shares::{Lots, Shares, SharesError};
pub use signal::{
    EventKind, NewsImportance, NewsKind, SignalDetection, SignalKind,
};
pub use time::{OccurredAt, TimeError, TradeDate};
