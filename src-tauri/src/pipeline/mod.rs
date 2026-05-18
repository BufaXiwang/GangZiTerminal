//! 后端流水线（pipeline 层）—— application use case 编排。
//!
//! v2 重构后 agent 模式：
//! 1. **chat**：用户消息触发 → 注入 active principles + active theses + 持仓上下文
//!    → agent loop 调工具（含 thesis / principle / account 写工具）→ 写 assistant message
//! 2. **reflection**（pipeline/agent/reflect.rs）：每交易日 15:30 自动触发 → 复盘 closed
//!    positions + active theses → propose_principle + update_thesis_state
//!
//! 子模块布局：
//! - `account` / `agent` / `chat` / `chat_attachments` / `history` / `news` / `stocks`：use case
//! - `market/`：行情刷新 / 大盘 / universe / K 线预热
//! - `scheduler`：后台 tick loops（reflection tick 在 adapters/reflection_scheduler.rs）
//! - `events` / `quotes_fetch` / `context` / `util`：跨 use case helper

pub mod account;
pub mod agent;
pub mod chat;
pub mod chat_attachments;
pub mod context;
pub mod events;
pub mod history;
pub mod market;
pub mod news;
pub mod quotes_fetch;
pub mod scheduler;
pub mod stocks;
pub mod util;
