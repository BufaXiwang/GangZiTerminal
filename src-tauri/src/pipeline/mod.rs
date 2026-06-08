//! Pipeline 层：use case + 后台任务 + 跨 infra 编排。
//!
//! Spec: docs/design/architecture.md §2
//!
//! 铁律：pipeline/ 不允许 use adapters。

pub mod account;
pub mod agent;
pub mod agent_runtime;
pub mod news;
pub mod quotes;
