//! Subscriptions —— 暴露 account 当前关注的 ts_code 列表给 quotes refresh 用。
//!
//! ```
//! subscribed_codes(app) = watchlist ∪ open positions  (去重，转成 ts_code)
//! ```
//!
//! watchlist 的所有权和 CRUD 都在 account；`pipeline/market_refresh.rs` 每 tick
//! 消费这里的订阅集合，合并 `quotes::core_indexes()` 后刷新 MARKET_SNAPSHOT。
//!
//! 依赖单向：本函数被 pipeline 调；不反向引用 quotes 内部。

use crate::db;
use crate::infrastructure::account::{watchlist, PositionRepo};
use std::collections::BTreeSet;
use tauri::AppHandle;

/// 返回当前 account 关注的 ts_code 列表（去重排序）。
///
/// 失败的子集（resolve 失败 / 找不到 ts_code）静默跳过——不影响其它项刷新。
pub fn subscribed_codes(app: &AppHandle) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();

    // 1. watchlist —— 用户自选股 + agent 添加
    for code in watchlist::list() {
        if let Some(ts) = db::resolve_stock_ts_code(app, code.as_str()) {
            set.insert(ts);
        }
    }

    // 2. open positions —— 当前 open 状态的持仓
    let repo = PositionRepo::new(app.clone());
    if let Ok(positions) = repo.list_open() {
        for p in positions {
            if let Some(ts) = db::resolve_stock_ts_code(app, p.code.as_str()) {
                set.insert(ts);
            }
        }
    }

    set.into_iter().collect()
}
