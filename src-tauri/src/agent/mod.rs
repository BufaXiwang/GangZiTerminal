//! Agent 子系统：取代旧的 codex 黑盒，提供
//! - 自管 tool-use 循环（loop.rs）
//! - 上下文窗口与 prompt cache 调度（context.rs）
//! - Provider 抽象（provider/）
//! - 本地工具注册（tools/）
//! - 观测落表（observer.rs）
//!
//! Pipeline 层只构造 `AgentRequest`，调用 `loop::run_agent` 拿事件流。
//! 所有 AI 调用都从这里走，prompt.rs 不再解析模型文本输出。

pub mod compact;
pub mod config;
pub mod context;
pub mod loop_;
pub mod observer;
pub mod provider;
pub mod tools;
pub mod types;

pub use loop_::{run_agent, RunSummary, SummarizeOptions};
