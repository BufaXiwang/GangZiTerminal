//! `MARKET_SNAPSHOT`——全市场（股票 + 指数 + 基金）实时行情快照。
//!
//! refresh loop 高频维护 Account subscriptions（自选股 + 持仓）和核心指数，
//! 旁路任务可低频补齐更大的市场集合。
//!
//! 之所以分两个：
//! - StockCode = 6 位数字 → 000001.SH（上证指数）和 000001.SZ（平安银行）会冲突
//! - snapshot 用 ts_code（带 SH/SZ/BJ 后缀）作为唯一键避歧义

use crate::domain::quotes::StockQuote;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

static SNAPSHOT: OnceLock<RwLock<HashMap<String, StockQuote>>> = OnceLock::new();

fn store() -> &'static RwLock<HashMap<String, StockQuote>> {
    SNAPSHOT.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 读单条——ts_code 形式 ("000001.SH")。
pub fn get(ts_code: &str) -> Option<StockQuote> {
    store().read().ok()?.get(ts_code).cloned()
}

/// 一次性导出当前所有 (ts_code, quote)——前端首次加载用。
pub fn snapshot_all() -> Vec<(String, StockQuote)> {
    let guard = match store().read() {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };
    guard.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// 批量覆盖写——旁路刷新拉完一批后调用。
pub fn put_batch(items: Vec<(String, StockQuote)>) {
    let mut guard = match store().write() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    for (ts, q) in items {
        guard.insert(ts, q);
    }
}

/// 当前 snapshot 大小。
pub fn len() -> usize {
    store().read().map(|g| g.len()).unwrap_or(0)
}
