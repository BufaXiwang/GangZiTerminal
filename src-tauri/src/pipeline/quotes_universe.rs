//! Quotes universe 公共查询 facade。
//!
//! 给其它 BC（特别是 Account 的 watchlist / subscriptions）一个不绕过 spec 的
//! 入口：spec architecture.md §3 要求 Account 「只读 Quotes snapshot 不直接调
//! provider / repository」。watchlist 校验和 subscribed code 解析需要查 stocks /
//! indexes / funds 静态档案，这是合法读取，但不能 `use
//! crate::infrastructure::quotes::repository::*`。
//!
//! 本模块是 quotes BC 在 pipeline 层暴露的公共 lookup API，供同层其它 BC 调用：
//!
//! ```text
//! pipeline/account/{update_watchlist,subscriptions}.rs
//!   -> pipeline::quotes_universe::*
//!     -> infrastructure::quotes::repository::*  // 仅本文件可见
//! ```

use crate::infrastructure::quotes::repository as qrepo;
use tauri::AppHandle;

/// 把任意 6 位代码 / ts_code resolve 成标准化 `ts_code`（含 `.SH/.SZ/.BJ` 后缀）。
/// 未在 stocks 档案中 → None。
pub fn resolve_stock_ts_code(app: &AppHandle, code_or_ts: &str) -> Option<String> {
    qrepo::resolve_stock_ts_code(app, code_or_ts)
}

/// 给定 ts_code 是否在 Quotes universe 已知（含股票 / 指数 / 基金）。
pub fn is_known_instrument(app: &AppHandle, ts_code: &str) -> bool {
    if qrepo::resolve_stock_ts_code(app, ts_code).is_some() {
        return true;
    }
    if qrepo::list_indexes(app)
        .unwrap_or_default()
        .iter()
        .any(|r| r.ts_code == ts_code)
    {
        return true;
    }
    if qrepo::list_listed_funds(app)
        .unwrap_or_default()
        .iter()
        .any(|r| r.ts_code == ts_code)
    {
        return true;
    }
    false
}
