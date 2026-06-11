//! 复权 K 线 in-memory cache — 现算 qfq / hfq 的结果按 series 缓存。
//!
//! Spec: docs/design/quotes-module.md §2 "本地复权计算（基于 TDX xdxr）"
//!
//! Key = `(ts_code, period, mode, xdxr_version, limit)`。`xdxr_version` 由调用方提供，
//! 用来在 xdxr 数据刷新后失效旧 cache（spec §2 "xdxr 事件刷新时整体失效"）。
//! `limit` 必须入 key：同一标的不同根数的请求是不同序列——否则首次缓存的长度会被
//! 后续任意 limit 的请求原样命中（实测 bug 2026-06-11：kline --limit 1/3/10 恒返 5 根）。
//!
//! 简单进程内 cache，无持久化；进程重启清空（spec 没要求持久化）。
//!
//! Spec drift (acknowledged)：D2 范围不实现"xdxr 刷新触发 cache 失效"——
//! 调用方可手动 `invalidate(ts_code)`；D3 再接入事件驱动失效。

use crate::domain::quotes::{Adjust, KlinePeriod, KlineSeries};
use crate::domain::shared::TsCode;
use std::collections::HashMap;
use std::sync::RwLock;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AdjustCacheKey {
    pub ts_code: String,
    pub period: KlinePeriod,
    pub adjust: Adjust,
    /// xdxr 版本号：由调用方提供（一般用 events.len() 或 SUM(fetched_at)）。
    pub xdxr_version: i64,
    /// 请求根数（load_kline_series 的 limit）。不同 limit = 不同序列，必须区分缓存。
    pub limit: u32,
}

#[derive(Default)]
pub struct AdjustCache {
    inner: RwLock<HashMap<AdjustCacheKey, KlineSeries>>,
}

impl AdjustCache {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    pub fn get(&self, key: &AdjustCacheKey) -> Option<KlineSeries> {
        self.inner.read().ok()?.get(key).cloned()
    }

    pub fn put(&self, key: AdjustCacheKey, series: KlineSeries) {
        if let Ok(mut g) = self.inner.write() {
            g.insert(key, series);
        }
    }

    /// 移除某 ts_code 的所有 cache 条目。xdxr 刷新时调用。
    pub fn invalidate(&self, ts_code: &TsCode) -> usize {
        if let Ok(mut g) = self.inner.write() {
            let before = g.len();
            g.retain(|k, _| k.ts_code != ts_code.as_str());
            before - g.len()
        } else {
            0
        }
    }

    /// 清空 cache。
    #[allow(dead_code)]
    pub fn clear(&self) {
        if let Ok(mut g) = self.inner.write() {
            g.clear();
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.read().map(|g| g.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::quotes::{KlinePeriod, KlineSeries};
    use crate::domain::shared::{Freshness, FreshnessStatus};

    fn fake_series() -> KlineSeries {
        KlineSeries {
            period: KlinePeriod::Day,
            adjust: Adjust::Qfq,
            points: Vec::new(),
            freshness: Freshness {
                status: FreshnessStatus::Fresh,
                captured_at: None,
                exchange_time: None,
                age_ms: None,
                source: None,
                warning: None,
            },
            warnings: Vec::new(),
        }
    }

    #[test]
    fn put_get_roundtrip() {
        let c = AdjustCache::new();
        let k = AdjustCacheKey {
            ts_code: "600519.SH".into(),
            period: KlinePeriod::Day,
            adjust: Adjust::Qfq,
            xdxr_version: 1,
            limit: 120,
        };
        assert!(c.get(&k).is_none());
        c.put(k.clone(), fake_series());
        assert!(c.get(&k).is_some());
    }

    #[test]
    fn invalidate_removes_only_target_ts_code() {
        let c = AdjustCache::new();
        c.put(
            AdjustCacheKey {
                ts_code: "600519.SH".into(),
                period: KlinePeriod::Day,
                adjust: Adjust::Qfq,
                xdxr_version: 1,
                limit: 120,
            },
            fake_series(),
        );
        c.put(
            AdjustCacheKey {
                ts_code: "000001.SZ".into(),
                period: KlinePeriod::Day,
                adjust: Adjust::Qfq,
                xdxr_version: 1,
                limit: 120,
            },
            fake_series(),
        );
        assert_eq!(c.len(), 2);
        let ts = TsCode::parse("600519.SH").unwrap();
        let removed = c.invalidate(&ts);
        assert_eq!(removed, 1);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn limit_mismatch_misses() {
        // 回归（2026-06-11）：limit 不入 key 时，首次缓存的长度会被任意 limit 命中
        // （kline --limit 1/3/10 恒返首次缓存的根数）。不同 limit 必须各自成键。
        let c = AdjustCache::new();
        let k120 = AdjustCacheKey {
            ts_code: "600519.SH".into(),
            period: KlinePeriod::Day,
            adjust: Adjust::Qfq,
            xdxr_version: 1,
            limit: 120,
        };
        c.put(k120.clone(), fake_series());
        let k3 = AdjustCacheKey { limit: 3, ..k120.clone() };
        assert!(c.get(&k3).is_none(), "不同 limit 不得命中同一缓存");
        assert!(c.get(&k120).is_some());
    }

    #[test]
    fn version_mismatch_misses() {
        let c = AdjustCache::new();
        let k1 = AdjustCacheKey {
            ts_code: "600519.SH".into(),
            period: KlinePeriod::Day,
            adjust: Adjust::Qfq,
            xdxr_version: 1,
            limit: 120,
        };
        c.put(k1.clone(), fake_series());
        let k2 = AdjustCacheKey {
            xdxr_version: 2,
            ..k1
        };
        assert!(c.get(&k2).is_none());
    }
}
