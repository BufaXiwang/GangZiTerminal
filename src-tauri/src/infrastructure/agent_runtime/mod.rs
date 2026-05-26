//! Agent Runtime persistence —— spec `agent-runtime-module.md`.
//!
//! 一个 repo 文件按聚合根分：
//! - `runs_repo`：AgentRun 生命周期
//! - `episodes_repo`：DecisionEpisode
//! - `trade_intents_repo`：TradeIntent + AgentOrderIntentIndex
//! - `reviews_repo`：DecisionReview
//! - `strategy_cards_repo`：StrategyCard upsert / 列表
//! - `event_consumption_repo`：跨模块事件 idempotency
//! - `news_buffer_repo`：news_analysis buffer

pub mod cancellation;
pub mod episodes_repo;
pub mod event_consumption_repo;
pub mod locks_ext;
pub mod news_buffer_repo;
pub mod reviews_repo;
pub mod runs_repo;
pub mod settings;
pub mod strategy_audit_repo;
pub mod strategy_cards_repo;
pub mod trade_intents_repo;
