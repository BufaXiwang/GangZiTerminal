//! TuShare 健康检查 service。
//!
//! Spec: docs/design/quotes-module.md §2 "TuShare 健康状态" + §5 "TuShare 健康探针"
//!
//! 协议：
//! - 启动 ping：进程启动调用 `initial_ping()`；超时 / HTTP 失败 / 鉴权失败 → `is_available = false`。
//! - 定期重试：`is_available = false` 时按 `recheck_interval` 定时重新 ping。
//! - 业务调用熔断：`record_failure()` 累计达 `max_consecutive_failures` 次时主动翻 false。
//! - token 缺失：直接 `unavailable("token_missing")`，不发起任何网络请求。
//!
//! `TushareClient` 不持有这份 state；调用方（`QuotesService`）持有 `Arc<TushareHealthCheck>`
//! 并在每个 TuShare provider 调用前 `is_available()`。

use crate::domain::quotes::{TushareHealthConfig, TushareHealthState};
use crate::infrastructure::quotes::tushare::client::TushareClient;
use chrono::Utc;
use std::sync::{Arc, RwLock};

/// 共享 health state + 探针 service。
pub struct TushareHealthCheck {
    state: Arc<RwLock<TushareHealthState>>,
    consecutive_failures: Arc<RwLock<u32>>,
    client: Arc<TushareClient>,
    config: TushareHealthConfig,
}

impl TushareHealthCheck {
    pub fn new(client: Arc<TushareClient>, config: TushareHealthConfig) -> Self {
        let state = if !client.has_token() {
            TushareHealthState::token_missing()
        } else {
            TushareHealthState::unknown()
        };
        Self {
            state: Arc::new(RwLock::new(state)),
            consecutive_failures: Arc::new(RwLock::new(0)),
            client,
            config,
        }
    }

    /// 进程启动时调用一次。token 缺失时直接 short-circuit，不发起网络。
    ///
    /// 网络异常 / 鉴权失败时只更新 state，不返回 Err（这是后台健康探针，不应中断启动）。
    pub async fn initial_ping(&self) {
        if !self.client.has_token() {
            return; // state 已在 new() 设为 token_missing
        }
        self.run_probe("initial_ping").await;
    }

    /// 业务调用成功时通知。重置失败计数；如果当前 unavailable，也翻回 available。
    pub fn record_success(&self) {
        let now = now_ms();
        if let Ok(mut counter) = self.consecutive_failures.write() {
            *counter = 0;
        }
        if let Ok(mut s) = self.state.write() {
            s.is_available = true;
            s.last_success_at = Some(now);
            s.last_error = None;
            s.next_recheck_at = None;
        }
    }

    /// 业务调用失败时通知。累计连续失败达阈值时主动熔断 → unavailable。
    pub fn record_failure(&self, err_msg: impl Into<String>) {
        let msg = err_msg.into();
        let max = self.config.max_consecutive_failures;
        let counter_now = {
            if let Ok(mut c) = self.consecutive_failures.write() {
                *c = c.saturating_add(1);
                *c
            } else {
                0
            }
        };
        if counter_now >= max {
            let now = now_ms();
            let next = now + self.config.recheck_interval.as_millis() as i64;
            if let Ok(mut s) = self.state.write() {
                s.is_available = false;
                s.last_error = Some(format!("circuit_open: {msg}"));
                s.next_recheck_at = Some(next);
            }
        }
    }

    /// 当前 state 快照。
    pub fn state(&self) -> TushareHealthState {
        self.state
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| TushareHealthState::default())
    }

    /// 便利方法：当前是否可用。
    pub fn is_available(&self) -> bool {
        self.state
            .read()
            .map(|g| g.is_available)
            .unwrap_or(false)
    }

    /// 由 scheduler 定时调用：检查 `next_recheck_at` 是否到点，到点则重新 ping。
    pub async fn recheck_if_due(&self) {
        if !self.client.has_token() {
            return;
        }
        let due = {
            let s = match self.state.read() {
                Ok(g) => g.clone(),
                Err(_) => return,
            };
            // available 状态下不主动重 ping（仅作为业务调用结果驱动）
            if s.is_available {
                return;
            }
            // token_missing 也不主动 ping
            if s.last_error.as_deref() == Some("token_missing") {
                return;
            }
            let now = now_ms();
            s.next_recheck_at.map(|t| now >= t).unwrap_or(true)
        };
        if due {
            self.run_probe("recheck").await;
        }
    }

    /// 执行一次健康探针：调 `trade_cal` 拿一天，作为最便宜的 TuShare call。
    async fn run_probe(&self, label: &str) {
        let now = now_ms();
        // 用一个固定的近期工作日（避免参数随时间漂移）— TuShare `trade_cal` 接受任意日期窗口。
        // 这里用 yesterday → today，单日窗口确保返回 ≤ 1 行。
        let today = chrono::Utc::now().date_naive();
        let yesterday = today - chrono::Duration::days(1);
        let s = yesterday.format("%Y%m%d").to_string();
        let e = today.format("%Y%m%d").to_string();
        match self.client.fetch_trade_cal(&s, &e).await {
            Ok(_) => {
                tracing::info!(target: "quotes.tushare.health", probe = label, "tushare health probe ok");
                if let Ok(mut counter) = self.consecutive_failures.write() {
                    *counter = 0;
                }
                if let Ok(mut st) = self.state.write() {
                    st.is_available = true;
                    st.last_ping_at = Some(now);
                    st.last_success_at = Some(now);
                    st.last_error = None;
                    st.next_recheck_at = None;
                }
            }
            Err(err) => {
                let msg = err.to_string();
                tracing::warn!(target: "quotes.tushare.health", probe = label, error = %msg, "tushare health probe failed");
                let next = now + self.config.recheck_interval.as_millis() as i64;
                if let Ok(mut st) = self.state.write() {
                    st.is_available = false;
                    st.last_ping_at = Some(now);
                    st.last_error = Some(msg);
                    st.next_recheck_at = Some(next);
                }
            }
        }
    }
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_with_token(token: Option<&str>) -> Arc<TushareClient> {
        Arc::new(TushareClient::new(token.map(String::from)).unwrap())
    }

    #[test]
    fn new_with_missing_token_starts_unavailable_token_missing() {
        let hc = TushareHealthCheck::new(client_with_token(None), TushareHealthConfig::default());
        let s = hc.state();
        assert!(!s.is_available);
        assert_eq!(s.last_error.as_deref(), Some("token_missing"));
        assert!(!hc.is_available());
    }

    #[test]
    fn new_with_token_starts_unknown_not_yet_available() {
        let hc = TushareHealthCheck::new(
            client_with_token(Some("fake")),
            TushareHealthConfig::default(),
        );
        let s = hc.state();
        assert!(!s.is_available);
        assert!(s.last_error.is_none()); // unknown，未 ping 过
    }

    #[test]
    fn record_success_flips_to_available_and_resets_counter() {
        let hc = TushareHealthCheck::new(
            client_with_token(Some("fake")),
            TushareHealthConfig::default(),
        );
        hc.record_failure("x");
        hc.record_success();
        assert!(hc.is_available());
        let s = hc.state();
        assert!(s.last_success_at.is_some());
        assert!(s.last_error.is_none());
    }

    #[test]
    fn record_failure_opens_circuit_after_max_consecutive() {
        let cfg = TushareHealthConfig {
            max_consecutive_failures: 3,
            ..TushareHealthConfig::default()
        };
        let hc = TushareHealthCheck::new(client_with_token(Some("fake")), cfg);
        // 先标 available
        hc.record_success();
        assert!(hc.is_available());
        hc.record_failure("e1");
        assert!(hc.is_available(), "1 failure should not open circuit");
        hc.record_failure("e2");
        assert!(hc.is_available(), "2 failures should not open circuit");
        hc.record_failure("e3");
        assert!(!hc.is_available(), "3rd failure should open circuit");
        let s = hc.state();
        assert!(s.last_error.as_deref().unwrap_or("").starts_with("circuit_open:"));
        assert!(s.next_recheck_at.is_some());
    }

    #[test]
    fn record_success_resets_failure_counter() {
        let cfg = TushareHealthConfig {
            max_consecutive_failures: 3,
            ..TushareHealthConfig::default()
        };
        let hc = TushareHealthCheck::new(client_with_token(Some("fake")), cfg);
        hc.record_success();
        hc.record_failure("e1");
        hc.record_failure("e2");
        hc.record_success(); // 重置
        hc.record_failure("e3");
        // 这是计数 reset 后第 1 次失败；不应熔断
        assert!(hc.is_available());
    }

    #[tokio::test]
    async fn initial_ping_short_circuits_when_token_missing() {
        let hc = TushareHealthCheck::new(client_with_token(None), TushareHealthConfig::default());
        hc.initial_ping().await;
        // state 应保持 token_missing
        let s = hc.state();
        assert!(!s.is_available);
        assert_eq!(s.last_error.as_deref(), Some("token_missing"));
        assert!(s.last_ping_at.is_none()); // 未真正发起 ping
    }

    #[tokio::test]
    async fn recheck_if_due_noop_when_token_missing() {
        let hc = TushareHealthCheck::new(client_with_token(None), TushareHealthConfig::default());
        hc.recheck_if_due().await;
        let s = hc.state();
        assert!(s.last_ping_at.is_none());
    }
}
