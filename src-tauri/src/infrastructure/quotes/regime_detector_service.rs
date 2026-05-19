//! Regime detector service——读上证指数近 60 日 K → 调 detect_regime → 缓存。
//!
//! 见 docs/design/agent-v3-expectation-driven.md § 8（regime 切换防"牛市学的 principle 熊市用"）。
//!
//! 缓存策略：5 分钟 TTL（regime 是慢变量，不需要实时计算；盘中过 5 分钟重算一次足够）。

use crate::domain::quotes::regime::{detect_regime, Regime};
use crate::domain::quotes::types::{AdjMode, KlinePeriod};
use crate::infrastructure::quotes::cache::kline_cache;
use std::sync::Mutex;
use std::time::Instant;
use tauri::AppHandle;

/// 上证指数 ts_code——所有 A 股 regime 的 reference index
const REFERENCE_INDEX_TS_CODE: &str = "000001.SH";
const CACHE_TTL_SECS: u64 = 300; // 5 分钟

struct CacheEntry {
    regime: Regime,
    at: Instant,
}

static CACHE: Mutex<Option<CacheEntry>> = Mutex::new(None);

/// 获取当前 regime。先看缓存；过期则从 KLINE_SNAPSHOT 读上证 60 日 K 重算。
/// 任何失败（缓存 / K 数据不足）→ 返 None（让调用方走"不知道 regime 就不过滤"逻辑）。
pub fn current(app: &AppHandle) -> Option<Regime> {
    // 1. 查缓存
    {
        let cache = CACHE.lock().ok()?;
        if let Some(entry) = cache.as_ref() {
            if entry.at.elapsed().as_secs() < CACHE_TTL_SECS {
                return Some(entry.regime);
            }
        }
    }

    // 2. 缓存过期/无 → 拉上证 60 日 K + 重算
    let rows = kline_cache::read_klines(
        app,
        REFERENCE_INDEX_TS_CODE,
        KlinePeriod::Day,
        AdjMode::Qfq,
        60,
    );
    if rows.len() < 60 {
        // K 线不够 60 日 → 无法判定，不缓存
        tracing::debug!(
            count = rows.len(),
            "regime detector: 上证 K 线不足 60 日，跳过"
        );
        return None;
    }
    // 注意：read_klines 默认返回 desc by date → 翻转成升序给 detect_regime
    let mut sorted = rows;
    sorted.sort_by(|a, b| a.date.cmp(&b.date));
    let closes: Vec<f64> = sorted.iter().map(|r| r.close).collect();
    let regime = detect_regime(&closes);

    // 3. 写缓存
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some(CacheEntry {
            regime,
            at: Instant::now(),
        });
    }
    Some(regime)
}

/// 强制刷新——清缓存。供测试 / 手动重算用。
pub fn invalidate() {
    if let Ok(mut cache) = CACHE.lock() {
        *cache = None;
    }
}
