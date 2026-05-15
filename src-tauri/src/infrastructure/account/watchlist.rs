//! Watchlist——用户 + agent 共同管理的自选股集合。
//!
//! 移自 `infrastructure::quotes::snapshot::watchlist`——它是 **账户层** 概念，
//! 不是 quotes 的：
//! - 用户通过 UI add/remove，agent 通过 tool 仅可 add（按 spec 决定）
//! - 后端 `account::subscriptions::subscribed_codes()` 把 watchlist + 持仓合成给 quotes refresh 用
//!
//! 内存：`OnceLock<RwLock<BTreeSet<StockCode>>>` 进程级单例
//! 持久化：`app_state[KEY_WATCHLIST]`（JSON Array）
//! 启动 hydrate 由 main.rs setup 阶段触发。

use crate::db;
use crate::domain::shared::StockCode;
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::{OnceLock, RwLock};
use tauri::AppHandle;

pub const KEY_WATCHLIST: &str = "gangzi-terminal.watchlist";

static WATCHLIST: OnceLock<RwLock<BTreeSet<StockCode>>> = OnceLock::new();

fn store() -> &'static RwLock<BTreeSet<StockCode>> {
    WATCHLIST.get_or_init(|| RwLock::new(BTreeSet::new()))
}

// ============================================================================
// 启动 hydrate
// ============================================================================

/// 进程启动时调一次——从 app_state 把 watchlist 灌进内存单例。
pub fn hydrate(app: &AppHandle) {
    if let Ok(Some(value)) = db::load_app_state_value(app, KEY_WATCHLIST) {
        if let Some(arr) = value.as_array() {
            let codes: BTreeSet<StockCode> = arr
                .iter()
                .filter_map(|v| v.as_str())
                .filter_map(|s| StockCode::new(s).ok())
                .collect();
            if let Ok(mut g) = store().write() {
                *g = codes;
            }
        }
    }
}

// ============================================================================
// 同步读
// ============================================================================

pub fn list() -> Vec<StockCode> {
    store()
        .read()
        .map(|g| g.iter().cloned().collect())
        .unwrap_or_default()
}

pub fn list_strings() -> Vec<String> {
    list()
        .into_iter()
        .map(|code| code.as_str().to_string())
        .collect()
}

pub fn contains(code: &StockCode) -> bool {
    store().read().map(|g| g.contains(code)).unwrap_or(false)
}

pub fn len() -> usize {
    store().read().map(|g| g.len()).unwrap_or(0)
}

// ============================================================================
// 写（带持久化）
// ============================================================================

pub fn add(app: &AppHandle, code: StockCode) {
    if let Ok(mut g) = store().write() {
        g.insert(code);
    }
    persist(app);
}

pub fn remove(app: &AppHandle, code: &StockCode) {
    if let Ok(mut g) = store().write() {
        g.remove(code);
    }
    persist(app);
}

pub fn replace(app: &AppHandle, codes: Vec<StockCode>) {
    if let Ok(mut g) = store().write() {
        *g = codes.into_iter().collect();
    }
    persist(app);
}

fn persist(app: &AppHandle) {
    let codes = list_strings();
    let _ = db::save_app_state_value(app, KEY_WATCHLIST, &Value::from(codes));
}
