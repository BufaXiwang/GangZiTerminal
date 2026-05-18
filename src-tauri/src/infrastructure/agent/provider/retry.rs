//! `RetryingProvider`——给任意 `ChatProvider` 套一层指数退避重试。
//!
//! 适合此 app 的语义：
//! - schedulers 24/7 跑，Eastmoney/Anthropic 偶发抽风不该让一次 briefing 直接挂——
//!   重试一次大概率能过。
//! - 但 chat 是用户在场的——指数退避总等待时间要有上限（max_attempts × max_delay
//!   决定）。当前默认 5 次 × 32s 最大 = ~60s 上限，可以接受。
//! - 我们只重试 **stream() 调用本身的失败**（HTTP 层）；流中途的错误（已经开始流式
//!   返回 token 又出错）不重试——重试会让 UI 看到重复 token。这是和 claude code
//!   一致的做法（除非引入 tombstone 机制）。
//!
//! 错误分类参见 [`classify`]——只对 `RateLimited` / `Transient` / 5xx 退避。

use crate::domain::agent::types::AgentRequest;
use crate::infrastructure::agent::provider::{ChatProvider, ProviderError, ProviderEvent};
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use std::sync::Arc;
use std::time::Duration;

/// 哪类错误值得重试。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryClass {
    /// 网络抖动/限流/5xx——退避后再试。
    Retry,
    /// 鉴权错/请求构造错/协议错/配置错——重试也是同一个错，直接 raise。
    Permanent,
}

fn classify(err: &ProviderError) -> RetryClass {
    match err {
        ProviderError::RateLimited(_) => RetryClass::Retry,
        ProviderError::Transient(_) => RetryClass::Retry,
        ProviderError::Request { status, .. } if *status >= 500 => RetryClass::Retry,
        // 4xx（401/403/400）+ Protocol + Config 都是确定性错——重试无意义
        _ => RetryClass::Permanent,
    }
}

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// 含首次尝试在内的总次数。max_attempts=1 等于禁用重试。
    pub max_attempts: u32,
    /// 第一次重试的等待时间。后续指数翻倍直到 max_delay。
    pub base_delay: Duration,
    /// 单次退避上限。
    pub max_delay: Duration,
    /// 在 [base..(base*(1+jitter))] 之间随机——避免多 client 雪崩同步。0 表示禁用。
    pub jitter_pct: f32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(32),
            jitter_pct: 0.25,
        }
    }
}

impl RetryPolicy {
    /// 第 attempt 次重试（1-indexed，attempt=1 是第二次尝试）的退避时长。
    fn delay_for(&self, attempt: u32) -> Duration {
        let factor = 1u64 << attempt.min(20).saturating_sub(1).max(0);
        let base_ms = (self.base_delay.as_millis() as u64).saturating_mul(factor);
        let capped_ms = base_ms.min(self.max_delay.as_millis() as u64);
        let jitter = if self.jitter_pct > 0.0 {
            // 用 system time 的纳秒做廉价 jitter——不需要密码学随机
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let frac = (nanos % 1000) as f32 / 1000.0; // 0..1
            (capped_ms as f32 * self.jitter_pct * frac) as u64
        } else {
            0
        };
        Duration::from_millis(capped_ms + jitter)
    }
}

pub struct RetryingProvider {
    inner: Arc<dyn ChatProvider>,
    policy: RetryPolicy,
}

impl RetryingProvider {
    pub fn new(inner: Arc<dyn ChatProvider>, policy: RetryPolicy) -> Self {
        Self { inner, policy }
    }
}

#[async_trait]
impl ChatProvider for RetryingProvider {
    async fn stream(
        &self,
        req: &AgentRequest,
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError> {
        let mut attempt: u32 = 0;
        loop {
            match self.inner.stream(req).await {
                Ok(s) => {
                    if attempt > 0 {
                        tracing::info!(attempt, "provider stream succeeded after retry");
                    }
                    return Ok(s);
                }
                Err(err) => {
                    if classify(&err) == RetryClass::Permanent {
                        return Err(err);
                    }
                    attempt += 1;
                    if attempt >= self.policy.max_attempts {
                        tracing::error!(
                            attempts = attempt,
                            error = %err,
                            "provider stream failed after all retries"
                        );
                        return Err(err);
                    }
                    let delay = self.policy.delay_for(attempt);
                    tracing::warn!(
                        attempt,
                        max = self.policy.max_attempts,
                        delay_ms = delay.as_millis() as u64,
                        error = %err,
                        "provider stream failed, will retry"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::types::{
        AgentOptions, ContextBudget, Message, PipelineKind, Role, StopReason, SystemBlock,
    };
    use crate::infrastructure::agent::provider::TokenUsage;
    use futures_util::stream::{self, BoxStream, StreamExt};
    use std::sync::Mutex;

    fn dummy_request() -> AgentRequest {
        AgentRequest {
            system: vec![SystemBlock {
                text: "x".into(),
                cache_control: false,
            }],
            tools: vec![],
            messages: vec![Message {
                role: Role::User,
                content: vec![],
            }],
            options: AgentOptions {
                model: "fake".into(),
                max_tokens: 1,
                temperature: None,
                top_p: None,
                thinking: None,
                effort: None,
                max_turns: 1,
                stop_sequences: vec![],
                tool_timeout_secs: None,
            },
            budget: ContextBudget {
                soft_limit_tokens: 1000,
                hard_limit_tokens: 2000,
                compact_keep_last_n: 1,
                max_search_calls: 1,
            },
            trigger_message_id: None,
            pipeline: PipelineKind::Chat,
        }
    }

    /// 一个脚本化 provider——每次 stream() 按 calls 顺序消耗一个错误或一个成功。
    struct ScriptedProvider {
        responses: Mutex<Vec<Result<&'static str, ProviderError>>>,
        call_count: Mutex<u32>,
    }
    #[async_trait]
    impl ChatProvider for ScriptedProvider {
        async fn stream(
            &self,
            _req: &AgentRequest,
        ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError>
        {
            *self.call_count.lock().unwrap() += 1;
            let mut guard = self.responses.lock().unwrap();
            if guard.is_empty() {
                return Err(ProviderError::Protocol("script exhausted".into()));
            }
            let next = guard.remove(0);
            match next {
                Ok(_text) => {
                    let events = vec![Ok(ProviderEvent::MessageComplete {
                        message: Message {
                            role: Role::Assistant,
                            content: vec![],
                        },
                        stop_reason: StopReason::EndTurn,
                    })];
                    Ok(stream::iter(events).boxed())
                }
                Err(e) => Err(e),
            }
        }
    }

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(4),
            jitter_pct: 0.0,
        }
    }

    #[tokio::test]
    async fn retries_on_transient_then_succeeds() {
        let scripted = Arc::new(ScriptedProvider {
            responses: Mutex::new(vec![
                Err(ProviderError::Transient("net flap".into())),
                Err(ProviderError::Transient("net flap".into())),
                Ok("hi"),
            ]),
            call_count: Mutex::new(0),
        });
        let provider: Arc<dyn ChatProvider> = scripted.clone();
        let retry = RetryingProvider::new(provider, fast_policy());
        let req = dummy_request();
        let result = retry.stream(&req).await;
        assert!(result.is_ok());
        assert_eq!(*scripted.call_count.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn retries_on_rate_limited() {
        let scripted = Arc::new(ScriptedProvider {
            responses: Mutex::new(vec![
                Err(ProviderError::RateLimited("slow down".into())),
                Ok("hi"),
            ]),
            call_count: Mutex::new(0),
        });
        let provider: Arc<dyn ChatProvider> = scripted.clone();
        let retry = RetryingProvider::new(provider, fast_policy());
        let _ = retry.stream(&dummy_request()).await.unwrap();
        assert_eq!(*scripted.call_count.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn retries_on_5xx() {
        let scripted = Arc::new(ScriptedProvider {
            responses: Mutex::new(vec![
                Err(ProviderError::Request {
                    status: 503,
                    body: "down".into(),
                }),
                Ok("hi"),
            ]),
            call_count: Mutex::new(0),
        });
        let provider: Arc<dyn ChatProvider> = scripted.clone();
        let retry = RetryingProvider::new(provider, fast_policy());
        let _ = retry.stream(&dummy_request()).await.unwrap();
        assert_eq!(*scripted.call_count.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn does_not_retry_on_4xx_auth_error() {
        let scripted = Arc::new(ScriptedProvider {
            responses: Mutex::new(vec![Err(ProviderError::Request {
                status: 401,
                body: "bad token".into(),
            })]),
            call_count: Mutex::new(0),
        });
        let provider: Arc<dyn ChatProvider> = scripted.clone();
        let retry = RetryingProvider::new(provider, fast_policy());
        let result = retry.stream(&dummy_request()).await;
        assert!(matches!(
            result,
            Err(ProviderError::Request { status: 401, .. })
        ));
        assert_eq!(*scripted.call_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn does_not_retry_on_protocol_error() {
        let scripted = Arc::new(ScriptedProvider {
            responses: Mutex::new(vec![Err(ProviderError::Protocol("malformed".into()))]),
            call_count: Mutex::new(0),
        });
        let provider: Arc<dyn ChatProvider> = scripted.clone();
        let retry = RetryingProvider::new(provider, fast_policy());
        let result = retry.stream(&dummy_request()).await;
        assert!(matches!(result, Err(ProviderError::Protocol(_))));
        assert_eq!(*scripted.call_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let scripted = Arc::new(ScriptedProvider {
            responses: Mutex::new(vec![
                Err(ProviderError::Transient("1".into())),
                Err(ProviderError::Transient("2".into())),
                Err(ProviderError::Transient("3".into())),
                Err(ProviderError::Transient("4".into())),
                Err(ProviderError::Transient("5".into())),
            ]),
            call_count: Mutex::new(0),
        });
        let provider: Arc<dyn ChatProvider> = scripted.clone();
        let retry = RetryingProvider::new(provider, fast_policy());
        let result = retry.stream(&dummy_request()).await;
        assert!(matches!(result, Err(ProviderError::Transient(_))));
        // max_attempts = 5：1 次尝试 + 4 次重试 = 5
        assert_eq!(*scripted.call_count.lock().unwrap(), 5);
    }

    #[test]
    fn delay_increases_exponentially_then_caps() {
        let p = RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(800),
            jitter_pct: 0.0,
        };
        // attempt=1 → 100ms, attempt=2 → 200, attempt=3 → 400, attempt=4 → 800 (capped),
        // attempt=5 → 800 (still capped)
        assert_eq!(p.delay_for(1), Duration::from_millis(100));
        assert_eq!(p.delay_for(2), Duration::from_millis(200));
        assert_eq!(p.delay_for(3), Duration::from_millis(400));
        assert_eq!(p.delay_for(4), Duration::from_millis(800));
        assert_eq!(p.delay_for(5), Duration::from_millis(800));
    }

    #[test]
    fn jitter_adds_within_pct() {
        let p = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1000),
            max_delay: Duration::from_millis(1000),
            jitter_pct: 0.25,
        };
        // attempt=1 → 1000ms base + 0..250ms jitter
        let d = p.delay_for(1);
        assert!(d >= Duration::from_millis(1000));
        assert!(d <= Duration::from_millis(1250));
    }
}
