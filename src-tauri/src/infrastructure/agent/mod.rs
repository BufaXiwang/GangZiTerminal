#![allow(dead_code, unused_imports)] // provider 实现侧完整能力面，部分按需启用

//! Infrastructure `agent`——Agent provider 实现 + Agent 持久化。
//!
//! - `provider/`：ChatProvider trait 实现（anthropic / openai 三个 wire format + retry 包装）
//! - `repository/`：chat_messages / agent_runs 持久化
//!
//! LLM tool registry 属于外部协议适配边界，放在 `adapters::agent_tools`，避免
//! infrastructure 反向依赖 pipeline / adapters。

pub mod provider;
pub mod repository;
