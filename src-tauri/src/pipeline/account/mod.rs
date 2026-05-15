//! Pipeline `account`——模拟账户用例编排层。
//!
//! 由 IPC adapter / scheduler / agent tool（屏蔽期暂不用）调用，**唯一的写入口**。
//!
//! 三个子模块：
//! - `service`：`AccountService`（含 Mutex 写锁）—— 5 个写操作 + snapshot 读
//! - `close`：批量平仓事务（reset / 未来风控扫描复用）
//! - `subscriptions`：暴露 `subscribed_codes()` 给 quotes refresh 用

pub mod close;
pub mod service;
pub mod subscriptions;

pub use service::{AccountService, OpenRequest};
pub use subscriptions::subscribed_codes;
