//! Domain 层——纯 domain 模型，无 I/O，无 Tauri，无外部副作用。
//!
//! 按 architecture.md § 9.1 DDD-lite 结构：
//! - `shared/`：跨 Bounded Context 复用的 newtype + value object
//! - `quotes/`：市场数据 BC
//! - `account/`（后续 phase）：模拟账户 BC
//! - `news/`（后续 phase）：资讯 BC
//! - `agent/`（后续 phase）：Agent 决策 BC
//!
//! Domain 模块**不引用** infrastructure / application / adapters。所有 I/O 在
//! domain 之外实现，通过 trait 在 domain 内定义契约。

pub mod account;
pub mod agent;
pub mod news;
pub mod quotes;
pub mod shared;
