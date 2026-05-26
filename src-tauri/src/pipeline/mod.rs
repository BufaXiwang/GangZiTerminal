//! 后端流水线（pipeline 层）—— application use case 编排。
//!
//! 子模块布局（对齐 docs/design/architecture.md）：
//! - `agent`：Agent Infra（loop / context / compact / tools 抽象）
//! - `agent_runtime`：Agent Runtime（run lifecycle / profile / packet / decisions / 路由 / locks）
//! - `account` / `chat` / `chat_attachments` / `history` / `news` / `stocks`：use case
//! - `market/`：行情刷新 / 大盘 / universe / K 线预热
//! - `scheduler`：后台 tick loops（news / market / kline warm / account snapshot）
//! - `events` / `quotes_fetch` / `context` / `util`：跨 use case helper

pub mod account;
pub mod agent;
pub mod agent_runtime;
pub mod chat;
pub mod chat_attachments;
pub mod context;
pub mod events;
pub mod history;
pub mod market;
pub mod news;
pub mod quotes_fetch;
pub mod quotes_universe;
pub mod scheduler;
pub mod stocks;
pub mod util;
