//! Runtime settings facade —— 类型化 getter/setter，所有硬编码阈值改读此处。
//!
//! Spec: agent-runtime-module.md §8 Runtime settings keys
//!
//! 契约（spec §8）：
//! - settings 是运行时配置，不属于 `InvestmentStrategy`，模型不能隐式改。
//! - **缺失用缺省**；**解析非法 → fail-closed（退回安全缺省）+ heartbeat（`tracing::warn!`）**，不 panic。
//!
//! 本层是 facade（key 常量 + 缺省 + 解析）；底层 kv 存取在 `AgentRuntimeRepo`。

use std::sync::Arc;

use chrono::Utc;

use crate::infrastructure::agent::runtime_repo::AgentRuntimeRepo;

// ---- key 常量（spec §8 表）-------------------------------------------------

// news buffer
pub const KEY_NEWS_AUTO_ANALYSIS_ENABLED: &str = "news_auto_analysis_enabled";
pub const KEY_NEWS_AGENT_BATCH_SIZE: &str = "news_agent_batch_size";
pub const KEY_NEWS_AGENT_MAX_WAIT_SECS: &str = "news_agent_max_wait_secs";
pub const KEY_NEWS_BUFFER_WINDOW_SECS: &str = "news_buffer_window_secs";

// account trigger 评估
pub const KEY_ACCOUNT_TRIGGER_EVAL_INTERVAL_SECS: &str = "account_trigger_eval_interval_secs";
pub const KEY_ACCOUNT_TRIGGER_EVAL_BATCH_SIZE: &str = "account_trigger_eval_batch_size";

// 复盘
pub const KEY_EOD_REVIEW_TIME: &str = "eod_review_time";
pub const KEY_REVIEW_MIN_SAMPLE_TRADES: &str = "review_min_sample_trades";

// token 护栏
pub const KEY_AGENT_RUN_MAX_TURNS: &str = "agent_run_max_turns";
pub const KEY_AGENT_RUN_TOKEN_BUDGET: &str = "agent_run_token_budget";
pub const KEY_AGENT_DAILY_TOKEN_BUDGET: &str = "agent_daily_token_budget";

// Infra 压缩 / 维护 cadence（本批仅占位，由各自模块消费；缺省见 spec §8）
pub const KEY_CONTEXT_SOFT_LIMIT_TOKENS: &str = "context_soft_limit_tokens";
pub const KEY_CONTEXT_SUMMARIZE_LIMIT_TOKENS: &str = "context_summarize_limit_tokens";
pub const KEY_CONTEXT_HARD_LIMIT_TOKENS: &str = "context_hard_limit_tokens";
pub const KEY_CONTEXT_COMPACT_CHANNEL_ID: &str = "agent_context_compact_channel_id";
pub const KEY_CONTEXT_COMPACT_MODEL: &str = "agent_context_compact_model";
pub const KEY_QUOTES_INTRADAY_REFRESH_TIME: &str = "quotes_intraday_refresh_time";
pub const KEY_QUOTES_CLOSE_REFRESH_TIME: &str = "quotes_close_refresh_time";
pub const KEY_NEWS_ARTICLE_WARM_TIME: &str = "news_article_warm_time";


// ---- 缺省 -------------------------------------------------------------------

pub const DEFAULT_NEWS_AUTO_ANALYSIS_ENABLED: bool = false;
pub const DEFAULT_NEWS_AGENT_BATCH_SIZE: u32 = 50;
pub const DEFAULT_NEWS_AGENT_MAX_WAIT_SECS: u64 = 600;
pub const DEFAULT_NEWS_BUFFER_WINDOW_SECS: i64 = 14_400;
pub const DEFAULT_ACCOUNT_TRIGGER_EVAL_INTERVAL_SECS: u64 = 10;
pub const DEFAULT_ACCOUNT_TRIGGER_EVAL_BATCH_SIZE: u32 = 200;
pub const DEFAULT_EOD_REVIEW_TIME: &str = "15:30 Asia/Shanghai";
pub const DEFAULT_REVIEW_MIN_SAMPLE_TRADES: u32 = 30;
pub const DEFAULT_AGENT_RUN_MAX_TURNS: u32 = 40;
pub const DEFAULT_AGENT_RUN_TOKEN_BUDGET: u32 = 200_000;
pub const DEFAULT_AGENT_DAILY_TOKEN_BUDGET: u64 = 5_000_000;

/// Runtime settings facade。包一份 repo；每个 getter 解析对应 key。
pub struct RuntimeSettings {
    repo: Arc<AgentRuntimeRepo>,
}

impl RuntimeSettings {
    pub fn new(repo: Arc<AgentRuntimeRepo>) -> Self {
        Self { repo }
    }

    // ---- 通用解析（fail-closed + heartbeat）-------------------------------

    /// 读 raw 值（缺失/IO 错 → None；IO 错 warn）。
    fn raw(&self, key: &str) -> Option<String> {
        match self.repo.get_setting(key) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "runtime.settings",
                    key,
                    error = %e,
                    "read setting failed; falling back to default"
                );
                None
            }
        }
    }

    /// 解析为 T；缺失或非法 → 缺省（非法时 warn = heartbeat 语义）。
    fn parse_or<T: std::str::FromStr>(&self, key: &str, default: T) -> T {
        match self.raw(key) {
            None => default,
            Some(s) => match s.parse::<T>() {
                Ok(v) => v,
                Err(_) => {
                    tracing::warn!(
                        target: "runtime.settings",
                        key,
                        raw = %s,
                        "invalid setting value; falling back to default (fail-closed)"
                    );
                    default
                }
            },
        }
    }

    /// bool 解析（接受 true/false/1/0，大小写不敏感）；非法 → 缺省 + warn。
    fn parse_bool_or(&self, key: &str, default: bool) -> bool {
        match self.raw(key) {
            None => default,
            Some(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" => true,
                "false" | "0" => false,
                _ => {
                    tracing::warn!(
                        target: "runtime.settings",
                        key,
                        raw = %s,
                        "invalid bool setting; falling back to default (fail-closed)"
                    );
                    default
                }
            },
        }
    }

    /// 字符串 getter（缺失返回缺省，不解析）。
    fn string_or(&self, key: &str, default: &str) -> String {
        self.raw(key).unwrap_or_else(|| default.to_string())
    }

    // ---- 类型化 getter（spec §8）------------------------------------------

    pub fn news_auto_analysis_enabled(&self) -> bool {
        self.parse_bool_or(KEY_NEWS_AUTO_ANALYSIS_ENABLED, DEFAULT_NEWS_AUTO_ANALYSIS_ENABLED)
    }
    pub fn news_agent_batch_size(&self) -> u32 {
        self.parse_or(KEY_NEWS_AGENT_BATCH_SIZE, DEFAULT_NEWS_AGENT_BATCH_SIZE)
    }
    pub fn news_agent_max_wait_secs(&self) -> u64 {
        self.parse_or(KEY_NEWS_AGENT_MAX_WAIT_SECS, DEFAULT_NEWS_AGENT_MAX_WAIT_SECS)
    }
    pub fn news_buffer_window_secs(&self) -> i64 {
        self.parse_or(KEY_NEWS_BUFFER_WINDOW_SECS, DEFAULT_NEWS_BUFFER_WINDOW_SECS)
    }
    pub fn account_trigger_eval_interval_secs(&self) -> u64 {
        self.parse_or(
            KEY_ACCOUNT_TRIGGER_EVAL_INTERVAL_SECS,
            DEFAULT_ACCOUNT_TRIGGER_EVAL_INTERVAL_SECS,
        )
    }
    pub fn account_trigger_eval_batch_size(&self) -> u32 {
        self.parse_or(
            KEY_ACCOUNT_TRIGGER_EVAL_BATCH_SIZE,
            DEFAULT_ACCOUNT_TRIGGER_EVAL_BATCH_SIZE,
        )
    }
    pub fn eod_review_time(&self) -> String {
        self.string_or(KEY_EOD_REVIEW_TIME, DEFAULT_EOD_REVIEW_TIME)
    }
    pub fn review_min_sample_trades(&self) -> u32 {
        self.parse_or(KEY_REVIEW_MIN_SAMPLE_TRADES, DEFAULT_REVIEW_MIN_SAMPLE_TRADES)
    }
    pub fn agent_run_max_turns(&self) -> u32 {
        self.parse_or(KEY_AGENT_RUN_MAX_TURNS, DEFAULT_AGENT_RUN_MAX_TURNS)
    }
    pub fn agent_run_token_budget(&self) -> u32 {
        self.parse_or(KEY_AGENT_RUN_TOKEN_BUDGET, DEFAULT_AGENT_RUN_TOKEN_BUDGET)
    }
    pub fn agent_daily_token_budget(&self) -> u64 {
        self.parse_or(KEY_AGENT_DAILY_TOKEN_BUDGET, DEFAULT_AGENT_DAILY_TOKEN_BUDGET)
    }


    // ---- setter（供前端开关 / 熔断状态写入）-------------------------------

    /// 写任意 key（字符串值）。
    pub fn set(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.repo.set_setting(key, value, Utc::now())
    }

    /// 写 news 自动分析开关。
    pub fn set_news_auto_analysis_enabled(&self, enabled: bool) -> rusqlite::Result<()> {
        self.set(
            KEY_NEWS_AUTO_ANALYSIS_ENABLED,
            if enabled { "true" } else { "false" },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::db::{run_migrations, AppDb};

    fn settings() -> RuntimeSettings {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        RuntimeSettings::new(Arc::new(AgentRuntimeRepo::new(db)))
    }

    #[test]
    fn defaults_when_missing() {
        let s = settings();
        assert_eq!(s.news_agent_batch_size(), DEFAULT_NEWS_AGENT_BATCH_SIZE);
        assert_eq!(s.news_auto_analysis_enabled(), DEFAULT_NEWS_AUTO_ANALYSIS_ENABLED);
        assert_eq!(s.agent_run_max_turns(), DEFAULT_AGENT_RUN_MAX_TURNS);
        assert_eq!(s.eod_review_time(), DEFAULT_EOD_REVIEW_TIME);
    }

    #[test]
    fn override_takes_effect() {
        let s = settings();
        s.set(KEY_NEWS_AGENT_BATCH_SIZE, "10").unwrap();
        s.set(KEY_EOD_REVIEW_TIME, "16:00 Asia/Shanghai").unwrap();
        assert_eq!(s.news_agent_batch_size(), 10);
        assert_eq!(s.eod_review_time(), "16:00 Asia/Shanghai");
    }

    #[test]
    fn invalid_value_falls_back_to_default() {
        let s = settings();
        // 非数字 → 缺省（fail-closed + warn heartbeat）。
        s.set(KEY_NEWS_AGENT_BATCH_SIZE, "not-a-number").unwrap();
        assert_eq!(s.news_agent_batch_size(), DEFAULT_NEWS_AGENT_BATCH_SIZE);
        // 非法 bool → 缺省。
        s.set(KEY_NEWS_AUTO_ANALYSIS_ENABLED, "maybe").unwrap();
        assert_eq!(s.news_auto_analysis_enabled(), DEFAULT_NEWS_AUTO_ANALYSIS_ENABLED);
    }

    #[test]
    fn bool_accepts_numeric_and_textual() {
        let s = settings();
        s.set(KEY_NEWS_AUTO_ANALYSIS_ENABLED, "1").unwrap();
        assert!(s.news_auto_analysis_enabled());
        s.set(KEY_NEWS_AUTO_ANALYSIS_ENABLED, "FALSE").unwrap();
        assert!(!s.news_auto_analysis_enabled());
        s.set_news_auto_analysis_enabled(true).unwrap();
        assert!(s.news_auto_analysis_enabled());
    }

}
