#![allow(dead_code, unused_imports)] // provider 实现侧完整能力面，部分按需启用

//! Infrastructure `agent`——Agent provider 实现 + Agent 持久化。
//!
//! - `provider/`：ChatProvider trait 实现（anthropic / openai 三个 wire format + retry 包装）
//! - `repository`：chat_messages / agent_episodes 持久化
//! - `principle_repo`：Principle aggregate 读写 + 健康度计数
//! - `health_metrics`：v2 机制健康度派生指标
//! - `seed_principles`：启动时注入 10 条 hand-written 投资原则
//!
//! LLM tool registry 属于外部协议适配边界，放在 `adapters::agent_tools`，避免
//! infrastructure 反向依赖 pipeline / adapters。

pub mod health_metrics;
pub mod heuristic_repo;
pub mod lesson_repo;
pub mod principle_repo;
pub mod provider;
pub mod repository;
pub mod seed_heuristics;
pub mod seed_principles;
pub mod seed_strategies;
pub mod signal_detection_repo;
pub mod strategy_repo;
