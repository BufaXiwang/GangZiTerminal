//! Adapters 层——Tauri / 前端协议边界。
//!
//! 把 domain / infrastructure 暴露给前端 invoke / event 通道。
//! 内部用 domain 类型，输出转 frontend-friendly JSON（DTO）。

pub mod account_commands;
pub mod agent_commands;
pub mod agent_tools;
pub mod app_commands;
pub mod app_state_commands;
pub mod chat_commands;
pub mod episode_commands;
pub mod expectation_commands;
pub mod market_commands;
pub mod news_commands;
pub mod principle_commands;
pub mod proxy_commands;
pub mod quotes_commands;
pub mod reflection_scheduler;
pub mod scan_scheduler;
pub mod thesis_commands;
