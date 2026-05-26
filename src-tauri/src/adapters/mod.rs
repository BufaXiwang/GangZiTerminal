//! Adapters 层：入站边界（Tauri command / Agent tool / 外部协议 DTO）。
//!
//! Spec: docs/design/architecture.md §2

pub mod account;
pub mod agent;
pub mod error;
pub mod news;
pub mod ping;
pub mod quotes;
