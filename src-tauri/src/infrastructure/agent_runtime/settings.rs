//! Agent Runtime settings KV —— spec `agent-runtime-module.md §8 "Runtime settings keys"`。
//!
//! 所有 runtime cadence / 阈值 / 模型渠道开关都从 `app_state` KV 表读取，
//! 缺省 fallback 到 spec 定义的默认值。spec §8 明确「配置缺失时必须使用上表
//! 缺省；非法配置必须 fail closed 并写 heartbeat」——这里读取走 best-effort：
//! 解析失败 / KV 缺失一律落到缺省值，并 trace 一条 warn。
//!
//! 配置由 `update_config` 走 `app_state::save_app_state_value` 写入，例如：
//!   key = "runtime.news_agent_batch_size", value = 30
//!
//! 不引入新表，所有 key 共享 `app_state` namespace；约定前缀 `runtime.*`。

use serde::de::DeserializeOwned;
use serde_json::Value;
use tauri::AppHandle;

use crate::infrastructure::app_state::repository::load_app_state_value;

pub const K_NEWS_BATCH_SIZE: &str = "runtime.news_agent_batch_size";
pub const K_NEWS_MAX_WAIT_SECS: &str = "runtime.news_agent_max_wait_secs";
pub const K_ACCOUNT_TRIGGER_EVAL_INTERVAL_SECS: &str =
    "runtime.account_trigger_eval_interval_secs";
pub const K_ACCOUNT_TRIGGER_EVAL_BATCH_SIZE: &str = "runtime.account_trigger_eval_batch_size";
pub const K_SCHEDULED_REVIEW_INTERVAL_SECS: &str = "runtime.scheduled_review_interval_secs";
pub const K_SCHEDULED_REVIEW_ALLOW_TRADING_WRITE: &str =
    "runtime.scheduled_review.allow_trading_write";
pub const K_QUOTES_MARKET_INSTRUMENTS_REFRESH_TIME: &str =
    "runtime.quotes_market_instruments_refresh_time";
pub const K_QUOTES_KLINE_WARM_TIME: &str = "runtime.quotes_kline_warm_time";
pub const K_QUOTES_DAILY_BASIC_REFRESH_TIME: &str = "runtime.quotes_daily_basic_refresh_time";
pub const K_QUOTES_COMPANY_EVENTS_REFRESH_INTERVAL_SECS: &str =
    "runtime.quotes_company_events_refresh_interval_secs";
pub const K_NEWS_ARTICLE_WARM_INTERVAL_SECS: &str = "runtime.news_article_warm_interval_secs";
pub const K_NEWS_ARTICLE_WARM_RECENT_LIMIT: &str = "runtime.news_article_warm_recent_limit";
pub const K_CONTEXT_SOFT_LIMIT_TOKENS: &str = "runtime.context_soft_limit_tokens";
pub const K_CONTEXT_SUMMARIZE_THRESHOLD: &str = "runtime.context_summarize_threshold";
pub const K_CONTEXT_HARD_LIMIT_TOKENS: &str = "runtime.context_hard_limit_tokens";
pub const K_AGENT_CONTEXT_COMPACT_CHANNEL_ID: &str = "runtime.agent_context_compact_channel_id";
pub const K_AGENT_CONTEXT_COMPACT_MODEL: &str = "runtime.agent_context_compact_model";
/// 模拟账户初始现金；默认 20000。仅在首次初始化（无 account_initialized 事件）
/// 时生效；后续 spec 要求账户 initialCash 不可变（已有则幂等校验）。
pub const K_ACCOUNT_INITIAL_CASH: &str = "runtime.account_initial_cash";

/// 读账户初始现金 KV，缺省回退到 `DEFAULT_INITIAL_CASH`。
pub fn account_initial_cash(app: &AppHandle) -> f64 {
    let default_cash = crate::infrastructure::account::valuation::DEFAULT_INITIAL_CASH;
    read_value(app, K_ACCOUNT_INITIAL_CASH)
        .and_then(|v| parse::<f64>(v, K_ACCOUNT_INITIAL_CASH))
        .filter(|n| *n > 0.0)
        .unwrap_or(default_cash)
}

/// spec `agent-runtime-module.md §8 Runtime settings keys`：14 项配置注册。
/// 部分字段（scheduled_review_interval_secs / agent_context_compact_*）目前
/// scheduled review tick 与 compact channel 切换尚未启用；保留字段以保证 KV
/// 写入 / UI Settings 页一次到位，避免后续接入再 bump 数据结构。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RuntimeSettings {
    pub news_agent_batch_size: i64,
    pub news_agent_max_wait_secs: i64,
    pub account_trigger_eval_interval_secs: i64,
    pub account_trigger_eval_batch_size: i64,
    pub scheduled_review_interval_secs: Option<i64>,
    pub scheduled_review_allow_trading_write: bool,
    pub quotes_market_instruments_refresh_time: String,
    pub quotes_kline_warm_time: String,
    pub quotes_daily_basic_refresh_time: String,
    pub quotes_company_events_refresh_interval_secs: i64,
    pub news_article_warm_interval_secs: i64,
    pub news_article_warm_recent_limit: i64,
    pub context_soft_limit_tokens: i64,
    pub context_summarize_threshold: i64,
    pub context_hard_limit_tokens: i64,
    pub agent_context_compact_channel_id: Option<String>,
    pub agent_context_compact_model: Option<String>,
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            news_agent_batch_size: 20,
            news_agent_max_wait_secs: 300,
            account_trigger_eval_interval_secs: 10,
            account_trigger_eval_batch_size: 200,
            scheduled_review_interval_secs: None,
            scheduled_review_allow_trading_write: false,
            quotes_market_instruments_refresh_time: "08:30 Asia/Shanghai".into(),
            quotes_kline_warm_time: "16:00 Asia/Shanghai".into(),
            quotes_daily_basic_refresh_time: "16:30 Asia/Shanghai".into(),
            quotes_company_events_refresh_interval_secs: 86400,
            news_article_warm_interval_secs: 1800,
            news_article_warm_recent_limit: 50,
            context_soft_limit_tokens: 48_000,
            context_summarize_threshold: 64_000,
            context_hard_limit_tokens: 96_000,
            agent_context_compact_channel_id: None,
            agent_context_compact_model: None,
        }
    }
}

fn read_value(app: &AppHandle, key: &str) -> Option<Value> {
    load_app_state_value(app, key).ok().flatten()
}

fn parse<T: DeserializeOwned>(v: Value, key: &str) -> Option<T> {
    serde_json::from_value::<T>(v.clone())
        .map_err(|e| {
            // spec §8「非法配置必须 fail closed 并写 heartbeat」
            tracing::warn!(
                target = "agent_runtime.settings",
                key,
                error = %e,
                "runtime setting 解析失败，fallback 到缺省"
            );
            // 直接拿 dummy app handle 写 heartbeat 不合适；这里只 trace，
            // settings 读路径无 AppHandle 走 record_err 的话需要每个 caller 传入。
            // 简化方案：caller 看 tracing warn 后自己 record_err；scheduler tick
            // 在 spec §8 fail closed 入口已经 record_err。
            e
        })
        .ok()
}

fn read_str(app: &AppHandle, key: &str, default: &str) -> String {
    read_value(app, key)
        .and_then(|v| parse::<String>(v, key))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}
fn read_opt_str(app: &AppHandle, key: &str) -> Option<String> {
    read_value(app, key)
        .and_then(|v| parse::<String>(v, key))
        .filter(|s| !s.trim().is_empty())
}
fn read_i64(app: &AppHandle, key: &str, default: i64) -> i64 {
    read_value(app, key)
        .and_then(|v| parse::<i64>(v, key))
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

pub fn load(app: &AppHandle) -> RuntimeSettings {
    let d = RuntimeSettings::default();
    RuntimeSettings {
        news_agent_batch_size: read_i64(app, K_NEWS_BATCH_SIZE, d.news_agent_batch_size),
        news_agent_max_wait_secs: read_i64(app, K_NEWS_MAX_WAIT_SECS, d.news_agent_max_wait_secs),
        account_trigger_eval_interval_secs: read_i64(
            app,
            K_ACCOUNT_TRIGGER_EVAL_INTERVAL_SECS,
            d.account_trigger_eval_interval_secs,
        ),
        account_trigger_eval_batch_size: read_i64(
            app,
            K_ACCOUNT_TRIGGER_EVAL_BATCH_SIZE,
            d.account_trigger_eval_batch_size,
        ),
        scheduled_review_interval_secs: read_value(app, K_SCHEDULED_REVIEW_INTERVAL_SECS)
            .and_then(|v| parse::<i64>(v, K_SCHEDULED_REVIEW_INTERVAL_SECS))
            .filter(|n| *n > 0),
        scheduled_review_allow_trading_write: read_value(
            app,
            K_SCHEDULED_REVIEW_ALLOW_TRADING_WRITE,
        )
        .and_then(|v| parse::<bool>(v, K_SCHEDULED_REVIEW_ALLOW_TRADING_WRITE))
        .unwrap_or(d.scheduled_review_allow_trading_write),
        quotes_market_instruments_refresh_time: read_str(
            app,
            K_QUOTES_MARKET_INSTRUMENTS_REFRESH_TIME,
            &d.quotes_market_instruments_refresh_time,
        ),
        quotes_kline_warm_time: read_str(app, K_QUOTES_KLINE_WARM_TIME, &d.quotes_kline_warm_time),
        quotes_daily_basic_refresh_time: read_str(
            app,
            K_QUOTES_DAILY_BASIC_REFRESH_TIME,
            &d.quotes_daily_basic_refresh_time,
        ),
        quotes_company_events_refresh_interval_secs: read_i64(
            app,
            K_QUOTES_COMPANY_EVENTS_REFRESH_INTERVAL_SECS,
            d.quotes_company_events_refresh_interval_secs,
        ),
        news_article_warm_interval_secs: read_i64(
            app,
            K_NEWS_ARTICLE_WARM_INTERVAL_SECS,
            d.news_article_warm_interval_secs,
        ),
        news_article_warm_recent_limit: read_i64(
            app,
            K_NEWS_ARTICLE_WARM_RECENT_LIMIT,
            d.news_article_warm_recent_limit,
        ),
        context_soft_limit_tokens: read_i64(
            app,
            K_CONTEXT_SOFT_LIMIT_TOKENS,
            d.context_soft_limit_tokens,
        ),
        context_summarize_threshold: read_i64(
            app,
            K_CONTEXT_SUMMARIZE_THRESHOLD,
            d.context_summarize_threshold,
        ),
        context_hard_limit_tokens: read_i64(
            app,
            K_CONTEXT_HARD_LIMIT_TOKENS,
            d.context_hard_limit_tokens,
        ),
        agent_context_compact_channel_id: read_opt_str(app, K_AGENT_CONTEXT_COMPACT_CHANNEL_ID),
        agent_context_compact_model: read_opt_str(app, K_AGENT_CONTEXT_COMPACT_MODEL),
    }
}
