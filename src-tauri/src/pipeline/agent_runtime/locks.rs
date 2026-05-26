//! In-flight lock & watchdog（骨架）。spec `agent-runtime-module.md §8`。
//!
//! 第一阶段：进程内 mutex 实现，按 spec 表注册的固定 lock key。
//! 持久化的 `AgentRuntimeEventConsumption` 表和 watchdog 未来落地。

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

fn inflight() -> &'static Mutex<HashSet<String>> {
    static INFLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    INFLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 尝试获取 lock；返回 false 表示已被占用。
pub fn try_acquire(key: &str) -> bool {
    let mut guard = inflight().lock().expect("inflight lock poisoned");
    guard.insert(key.to_string())
}

pub fn release(key: &str) {
    let mut guard = inflight().lock().expect("inflight lock poisoned");
    guard.remove(key);
}

/// RAII helper：drop 时自动 release。
pub struct InflightGuard {
    key: String,
    held: bool,
}

impl InflightGuard {
    pub fn acquire(key: impl Into<String>) -> Option<Self> {
        let key = key.into();
        if try_acquire(&key) {
            Some(Self { key, held: true })
        } else {
            None
        }
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if self.held {
            release(&self.key);
        }
    }
}
