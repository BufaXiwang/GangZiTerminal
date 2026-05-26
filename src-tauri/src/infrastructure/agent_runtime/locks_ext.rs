//! Spec `agent-runtime-module.md §8` in-flight lock key 常量全集。
//! 当前已用：`LOCK_NEWS_BATCH`（scheduler::news_buffer_loop）、`LOCK_ACCOUNT_TRIGGER_EVAL`
//! （scheduler::refresh_account_snapshot）、`account_trigger_run`（router）。
//! 其余 quotes / news / scheduled_review 等 key 留作 spec canonical 注册集，
//! 对应 refresh / warm / review loop 接入时启用。

#![allow(dead_code)] // spec §8 canonical lock 全集；部分 loop 接入后启用

// Agent runtime 侧
pub const LOCK_NEWS_BATCH: &str = "agent.news_batch";
pub const LOCK_SCHEDULED_REVIEW: &str = "agent.scheduled_review";

// Account 侧
pub const LOCK_ACCOUNT_TRIGGER_EVAL: &str = "account.trigger_eval";

// Quotes 侧
pub const LOCK_QUOTES_SUBSCRIBED_REFRESH: &str = "quotes.subscribed_refresh";
pub const LOCK_QUOTES_UNIVERSE_REFRESH: &str = "quotes.universe_refresh";
pub const LOCK_QUOTES_KLINE_REFRESH: &str = "quotes.kline_refresh";
pub const LOCK_QUOTES_DAILY_BASIC_REFRESH: &str = "quotes.daily_basic_refresh";
pub const LOCK_QUOTES_COMPANY_EVENTS_REFRESH: &str = "quotes.company_events_refresh";
pub const LOCK_QUOTES_MARKET_INSTRUMENTS_REFRESH: &str = "quotes.market_instruments_refresh";

// News 侧
pub const LOCK_NEWS_ARTICLE_WARM: &str = "news.article_warm";

pub fn account_trigger_run(trigger_id: &str) -> String {
    format!("agent.account_trigger:{trigger_id}")
}

pub fn quotes_close_snapshot(trade_date: &str) -> String {
    format!("quotes.close_snapshot:{trade_date}")
}
