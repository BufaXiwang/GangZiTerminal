//! Pipeline `agent`——Agent Infra：LLM Agent 执行底座（消息 / 上下文 / 工具协议 / 基础 loop）。
//!
//! 模块结构（对齐 docs/design/agent-infra-module.md）：
//! - `loop_`：tool-use 主循环（[`run_agent`]）
//! - `tools`：本地工具抽象（`Tool` trait / `ToolRegistry` / `ToolContext`）。具体工具实现在
//!   `adapters::agent_tools`——pipeline 只依赖抽象。
//! - `observer`：AgentEvent 转发到 Tauri emit + 落 agent_episodes 表。
//! - `context`：上下文压缩决策（soft / hard limit 触发）。
//! - `compact`：summarize tier 执行（用便宜模型压缩历史）。
//! - `config`：渠道配置 + Tauri command 读写。
//! - `prompt`：identity / instructions / context 构造。
//!
//! 产品级 Agent 应用层（run lifecycle / profile / decision episode / packet / 跨模块路由）
//! 在 [`crate::pipeline::agent_runtime`]。

pub mod compact;
pub mod config;
pub mod context;
pub mod loop_;
pub mod observer;
pub mod prompt;
pub mod tools;

pub use loop_::{run_agent, RunSummary, SummarizeOptions};
