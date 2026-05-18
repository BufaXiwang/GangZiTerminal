//! 后端流水线——目前只剩 chat（briefing/review 已下线）。
//!
//! chat 流程：
//! 1. 立刻写 user message（emit chat-message-appended，UI 即刻渲染）
//! 2. 读上下文（行情/持仓/记忆/学习/最近消息）
//! 3. 构 AgentRequest（identity+instructions 进 system，上下文+用户输入进 user）
//! 4. 启 agent_run（agent_runs 表先插一行）
//! 5. spawn forwarder：把 AgentEvent 流转发给前端 + 累计文本
//! 6. await run_agent → 拿 RunSummary + 最终文本
//! 7. 写 assistant message + finalize agent_runs
//!
//! Memory 更新由 agent 通过 update_memory / remove_memory 工具自己写。
//!
//! 子模块布局：
//! - `account` / `agent` / `chat` / `chat_attachments` / `history` / `news` / `stocks`：use case
//! - `market/`：行情刷新 / 大盘 / universe / K 线预热
//! - `scheduler`：后台 tick loops
//! - `events` / `memory` / `quotes_fetch` / `context` / `util`：被多个 use case 复用的 helper

pub mod account;
pub mod agent;
pub mod chat;
pub mod chat_attachments;
pub mod context;
pub mod events;
pub mod history;
pub mod market;
pub mod memory;
pub mod news;
pub mod quotes_fetch;
pub mod scheduler;
pub mod stocks;
pub mod util;
