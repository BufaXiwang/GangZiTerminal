//! Pipeline `agent`——Agent 决策子域的用例编排层。
//!
//! - `loop_`：tool-use 主循环（[`run_agent`]）
//! - `observer`：AgentEvent 转发到 Tauri emit + 落 agent_runs 表
//! - `context`：上下文压缩决策（soft / hard limit 触发）
//! - `compact`：summarize tier 执行（叫便宜模型压缩历史）
//! - `config`：N 渠道配置 + PipelineAssignments + Tauri command 读写
//!
//! Tauri command 入口在 `adapters::agent_commands`（薄包装）；config 本身保留业务逻辑。

pub mod compact;
pub mod config;
pub mod context;
pub mod loop_;
pub mod observer;
pub mod prompt;

pub use loop_::{run_agent, RunSummary, SummarizeOptions};
