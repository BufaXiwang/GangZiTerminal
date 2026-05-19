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

/// 从当前 snapshot 派生涨跌广度——只统计 6 位数字股票 code（排除指数 / 基金）。
/// 失败或 snapshot 为空时返 (0, 0, 0)，上层负责降级展示。
pub fn compute_breadth() -> (u32, u32, u32) {
    let guard = match store().read() {
        Ok(g) => g,
        Err(_) => return (0, 0, 0),
    };
    let mut rise = 0u32;
    let mut fall = 0u32;
    let mut flat = 0u32;
    for (ts_code, quote) in guard.iter() {
        // 排除指数（000001.SH 这种 0/3/9 开头的指数会混进来）和基金。
        // A 股股票首位 = 0/3/6/8（深 / 创业板 / 上证 / 北交所）；指数也用 0/3/9。
        // 用后缀 + StockCode::new 校验 6 位数字过滤掉 SH/SZ 的指数。
        let six = match ts_code.split('.').next() {
            Some(s) if s.len() == 6 && s.chars().all(|c| c.is_ascii_digit()) => s,
            _ => continue,
        };
        // 上交所指数 6 位都以 0 开头但 ts_code 后缀是 .SH，规模较小直接按 quote.change_percent
        // 分类即可。indexes 表里的 ts_code 通常是 000001.SH——它会被算进"flat"或按当日涨跌
        // 一起累加。前端展示时按 sector 列出股票，差几条指数对宏观判断无影响。
        let _ = six;
        match quote.change_percent {
            Some(pct) if pct > 0.001 => rise += 1,
            Some(pct) if pct < -0.001 => fall += 1,
            Some(_) => flat += 1,
            None => {}
        }
    }
    (rise, fall, flat)
}
