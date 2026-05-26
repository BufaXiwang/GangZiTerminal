//! Agent run 协作式取消（cooperative cancellation）—— spec
//! `agent-runtime-module.md §9 cancel_agent_run`。
//!
//! 全局 `HashMap<run_id, Arc<AtomicBool>>`：cancel_agent_run 在调用时把对应
//! flag 置 true；agent loop 在每个 turn 入口 / tool dispatch 之前查询；
//! 命中后停止后续 turn，已开始的 tool / provider 流允许跑完。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

fn registry() -> &'static Mutex<HashMap<String, Arc<AtomicBool>>> {
    static REG: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 注册 run 的 cancellation token；返回的 token 在 loop 内每个 turn 入口 check。
pub fn register(run_id: &str) -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    let mut g = registry().lock().expect("cancellation registry poisoned");
    g.insert(run_id.to_string(), flag.clone());
    flag
}

/// loop 跑完 / failed / errored 后调用 —— 释放 entry，避免内存泄漏。
pub fn unregister(run_id: &str) {
    let mut g = registry().lock().expect("cancellation registry poisoned");
    g.remove(run_id);
}

/// `cancel_agent_run` 命令调用。
pub fn cancel(run_id: &str) -> bool {
    let g = registry().lock().expect("cancellation registry poisoned");
    if let Some(flag) = g.get(run_id) {
        flag.store(true, Ordering::SeqCst);
        true
    } else {
        false
    }
}

/// loop 在 turn 入口检查。
pub fn is_cancelled(flag: &Arc<AtomicBool>) -> bool {
    flag.load(Ordering::SeqCst)
}

/// 直接按 run_id 查；loop 结束后 wrapper 用它判断是否落 Cancelled。
pub fn is_cancelled_by_id(run_id: &str) -> bool {
    let g = registry().lock().expect("cancellation registry poisoned");
    g.get(run_id)
        .map(|f| f.load(Ordering::SeqCst))
        .unwrap_or(false)
}
