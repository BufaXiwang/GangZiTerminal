//! TuShare 健康状态。
//!
//! Spec: docs/design/quotes-module.md §2 "TuShare 健康状态"
//!
//! 纯 domain 类型 + 配置值。无 I/O / 无 provider 依赖。
//! 实际探针 / 重试 / 熔断 service 见 `infrastructure::quotes::tushare::health`。

use crate::domain::shared::TimestampMs;
use std::time::Duration;

/// TuShare 健康快照。所有 TuShare provider 调用前必须先检查 `is_available`。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct TushareHealthState {
    pub is_available: bool,
    pub last_ping_at: Option<TimestampMs>,
    pub last_success_at: Option<TimestampMs>,
    pub last_error: Option<String>,
    pub next_recheck_at: Option<TimestampMs>,
}

impl Default for TushareHealthState {
    fn default() -> Self {
        Self {
            is_available: false,
            last_ping_at: None,
            last_success_at: None,
            last_error: None,
            next_recheck_at: None,
        }
    }
}

impl TushareHealthState {
    /// 启动时初始值：未 ping，未知。
    pub fn unknown() -> Self {
        Self::default()
    }

    /// token 缺失，直接进 unavailable 终态（无网络调用必要）。
    pub fn token_missing() -> Self {
        Self {
            is_available: false,
            last_ping_at: None,
            last_success_at: None,
            last_error: Some("token_missing".to_string()),
            next_recheck_at: None,
        }
    }

    /// 构造一个带错误信息的 unavailable 快照。
    pub fn unavailable_with_error(msg: impl Into<String>) -> Self {
        Self {
            is_available: false,
            last_ping_at: None,
            last_success_at: None,
            last_error: Some(msg.into()),
            next_recheck_at: None,
        }
    }
}

/// 健康检查 service 配置（不属于运行时状态本身）。
#[derive(Clone, Debug, PartialEq)]
pub struct TushareHealthConfig {
    /// `is_available = false` 后多久重新 ping（spec 默认 1 小时）。
    pub recheck_interval: Duration,
    /// 连续业务失败多少次后主动翻 `is_available = false`（spec 默认 3）。
    pub max_consecutive_failures: u32,
    /// 启动 ping 单次超时上限（避免 setup 阻塞，默认 5 秒）。
    pub initial_ping_timeout: Duration,
}

impl Default for TushareHealthConfig {
    fn default() -> Self {
        Self {
            recheck_interval: Duration::from_secs(3600),
            max_consecutive_failures: 3,
            initial_ping_timeout: Duration::from_secs(5),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_unknown_and_unavailable() {
        let s = TushareHealthState::default();
        assert!(!s.is_available);
        assert!(s.last_error.is_none());
        assert!(s.last_ping_at.is_none());
    }

    #[test]
    fn token_missing_carries_signal() {
        let s = TushareHealthState::token_missing();
        assert!(!s.is_available);
        assert_eq!(s.last_error.as_deref(), Some("token_missing"));
    }

    #[test]
    fn unavailable_with_error_records_msg() {
        let s = TushareHealthState::unavailable_with_error("http timeout");
        assert!(!s.is_available);
        assert_eq!(s.last_error.as_deref(), Some("http timeout"));
    }

    #[test]
    fn default_config_matches_spec() {
        let c = TushareHealthConfig::default();
        assert_eq!(c.recheck_interval.as_secs(), 3600);
        assert_eq!(c.max_consecutive_failures, 3);
    }
}
