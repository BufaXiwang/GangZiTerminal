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

/// 进程启动时调一次——把 watchlist 灌进内存单例。
///
/// 优先读 `app_state` KV 缓存（spec §2 派生读模型的运行时缓存）；
/// KV 缺失时**回退**到 `account_events` 流回放（spec §2 真源）：
/// `watchlist_added` / `watchlist_note_updated` 进集合，
/// `watchlist_removed` 退集合。
pub fn hydrate(app: &AppHandle) {
    if let Ok(Some(value)) =
        crate::infrastructure::app_state::load_app_state_value(app, KEY_WATCHLIST)
    {
        if let Some(arr) = value.as_array() {
            let codes: BTreeSet<StockCode> = arr
                .iter()
                .filter_map(|v| v.as_str())
                .filter_map(|s| StockCode::new(s).ok())
                .collect();
            if !codes.is_empty() {
                if let Ok(mut g) = store().write() {
                    *g = codes;
                }
                return;
            }
        }
    }
    // KV 缺失或空 → 从 account_events 真源回放重建。
    let codes = derive_from_events(app);
    if let Ok(mut g) = store().write() {
        *g = codes;
    }
    persist(app);
}

/// spec §2 watchlist 派生：扫 account_events 按 occurred_at 升序回放
/// add/note_updated/removed，得到当前 watchlist 集合。
fn derive_from_events(app: &AppHandle) -> BTreeSet<StockCode> {
    use crate::infrastructure::db::{migrate, open_database};
    let mut set: BTreeSet<StockCode> = BTreeSet::new();
    let Ok(c) = open_database(app) else {
        return set;
    };
    if migrate(&c).is_err() {
        return set;
    }
    let Ok(mut stmt) = c.prepare(
        "select event_type, ts_code from account_events
         where event_type in ('watchlist_added','watchlist_note_updated','watchlist_removed')
           and ts_code is not null
         order by occurred_at asc",
    ) else {
        return set;
    };
    let mut rows = match stmt.query([]) {
        Ok(r) => r,
        Err(_) => return set,
    };
    while let Ok(Some(r)) = rows.next() {
        let et: String = match r.get(0) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let ts_code: String = match r.get(1) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let Ok(code) = StockCode::new(&ts_code) else {
            continue;
        };
        match et.as_str() {
            "watchlist_added" | "watchlist_note_updated" => {
                set.insert(code);
            }
            "watchlist_removed" => {
                set.remove(&code);
            }
            _ => {}
        }
    }
    set
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
    if let Err(e) = crate::infrastructure::app_state::save_app_state_value(
        app,
        KEY_WATCHLIST,
        &Value::from(codes.clone()),
    ) {
        // KV 缓存写失败不破坏内存状态，下次 hydrate 会从 account_events 真源回放；
        // 但必须显式可观测，便于运维介入（spec §2 真源仍在 account_events 流）。
        tracing::error!(
            target = "account.watchlist",
            error = %e,
            codes_count = codes.len(),
            "watchlist KV persist 失败；内存状态保留，重启时由 account_events 流恢复"
        );
    }
}
