//! Pipeline `agent`——Agent 决策子域的用例编排层。
//!
//! - `loop_`：tool-use 主循环（[`run_agent`]）
//! - `tools`：本地工具抽象（`Tool` trait / `ToolRegistry` / `ToolContext`）。
//!   具体工具实现在 `adapters::agent_tools`——pipeline 只依赖抽象。
//! - `observer`：AgentEvent 转发到 Tauri emit + 落 agent_episodes 表
//! - `context`：上下文压缩决策（soft / hard limit 触发）
//! - `compact`：summarize tier 执行（叫便宜模型压缩历史）
//! - `config`：N 渠道配置 + PipelineAssignments + Tauri command 读写
//!
//! Tauri command 入口在 `adapters::agent_commands`（薄包装）；config 本身保留业务逻辑。

pub mod compact;
pub mod config;
pub mod context;
pub mod expectation_review;
pub mod heuristic_emerge;
pub mod loop_;
pub mod observer;
pub mod prompt;
pub mod reflect;
pub mod scan;
pub mod subagent;
pub mod tools;

pub use loop_::{run_agent, RunSummary, SummarizeOptions};
pub use subagent::{run_subagent, SubAgentError, SubAgentResult, SubAgentType};
