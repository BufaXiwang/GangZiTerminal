//! Subscriptions —— 暴露 account 当前关注的 ts_code 列表给 quotes refresh 用。
//!
//! spec account-module.md §5：
//! ```
//! subscribed_codes(app) = watchlist ∪ open_positions ∪ pending_orders  (去重，转成 ts_code)
//! ```
//!
//! watchlist 的所有权和 CRUD 都在 account；`pipeline/market_refresh.rs` 每 tick
//! 消费这里的订阅集合，合并 `quotes::core_indexes()` 后刷新 MARKET_SNAPSHOT。
//!
//! 依赖单向：spec architecture.md §3「Account 只读 Quotes snapshot 不直接调
//! repository」；本函数仅通过 [`crate::pipeline::quotes_universe`] facade resolve。

use crate::infrastructure::account::{orders_repo, watchlist, PositionRepo};
use crate::pipeline::quotes_universe;
use std::collections::BTreeSet;
use tauri::AppHandle;

/// 返回当前 account 关注的 ts_code 列表（去重排序）。
///
/// 失败的子集（resolve 失败 / 找不到 ts_code）静默跳过——不影响其它项刷新。
pub fn subscribed_codes(app: &AppHandle) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();

    // 1. watchlist —— 用户自选股 + agent 添加
    for code in watchlist::list() {
        if let Some(ts) = quotes_universe::resolve_stock_ts_code(app, code.as_str()) {
            set.insert(ts);
        }
    }

    // 2. open positions —— 当前 open 状态的持仓
    let repo = PositionRepo::new(app.clone());
    if let Ok(positions) = repo.list_open() {
        for p in positions {
            if let Some(ts) = quotes_universe::resolve_stock_ts_code(app, p.code.as_str()) {
                set.insert(ts);
            }
        }
    }

    // 3. pending orders —— spec §5：subscribed_codes 包含 pending order 的标的
    if let Ok(orders) = orders_repo::list_active(app, 500, 0) {
        for o in orders {
            set.insert(o.ts_code.as_str().to_string());
        }
    }

    set.into_iter().collect()
}
