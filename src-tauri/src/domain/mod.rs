//! Domain 层——纯 domain 模型，无 I/O，无 Tauri，无外部副作用。
//!
//! 按 architecture.md § 9.1 DDD-lite 结构：
//! - `shared/`：跨 Bounded Context 复用的 newtype + value object
//! - `quotes/`：市场数据 BC
//! - `account/`：模拟账户 BC（aggregate / events / cash / rules / sizing / snapshot / thesis）
//! - `news/`：资讯 BC
//! - `agent/`：Agent 决策 BC（canonical wire types + Principle aggregate）
//!
//! Domain 模块**不引用** infrastructure / pipeline / adapters。所有 I/O 在
//! domain 之外实现，通过 trait 在 domain 内定义契约。

pub mod account;
pub mod agent;
pub mod news;
pub mod quotes;
pub mod shared;
