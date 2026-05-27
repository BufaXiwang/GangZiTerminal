//! `MARKET_SNAPSHOT` 进程内 snapshot cache。
//!
//! Spec: docs/design/quotes-module.md §2
//!
//! 语义：
//! - 进程内 cache，不是持久化真源。
//! - 单槽：以 `tsCode` 为 key。
//! - 交易日切换时不要求立即清空旧槽位；读取路径必须按 eligible trade date 校验。
//! - 写入路径在 provider 成功 normalize 后写入此 cache。

use crate::domain::quotes::StockQuote;
use crate::domain::shared::{OccurredAt, TradeDate, TsCode};
use std::collections::HashMap;
use std::sync::RwLock;

#[derive(Debug, Clone)]
pub struct CachedSnapshot {
    pub quote: StockQuote,
    pub captured_at: OccurredAt,
    pub trade_date: TradeDate,
    pub source: String,
}

#[derive(Default)]
pub struct SnapshotCache {
    inner: RwLock<HashMap<TsCode, CachedSnapshot>>,
}

impl SnapshotCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&self, snap: CachedSnapshot) {
        let key = snap.quote.ts_code.clone();
        let mut g = self.inner.write().expect("snapshot cache poisoned");
        g.insert(key, snap);
    }

    pub fn put_many<I: IntoIterator<Item = CachedSnapshot>>(&self, snaps: I) {
        let mut g = self.inner.write().expect("snapshot cache poisoned");
        for s in snaps {
            g.insert(s.quote.ts_code.clone(), s);
        }
    }

    pub fn get(&self, ts_code: &TsCode) -> Option<CachedSnapshot> {
        let g = self.inner.read().expect("snapshot cache poisoned");
        g.get(ts_code).cloned()
    }

    pub fn get_many(&self, ts_codes: &[TsCode]) -> Vec<Option<CachedSnapshot>> {
        let g = self.inner.read().expect("snapshot cache poisoned");
        ts_codes.iter().map(|c| g.get(c).cloned()).collect()
    }

    pub fn snapshot_all(&self) -> Vec<CachedSnapshot> {
        let g = self.inner.read().expect("snapshot cache poisoned");
        g.values().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.inner.read().expect("snapshot cache poisoned").len()
    }

    /// 清除指定 ts_code 的 cache 槽位。
    ///
    /// 用于 `MarketInstrument.category` 变化时（universe refresh 检测到 category 变更）让旧的
    /// snapshot 不再被 list_market / scan_market 读到（避免类别错位）。
    pub fn invalidate(&self, ts_code: &TsCode) {
        let mut g = self.inner.write().expect("snapshot cache poisoned");
        g.remove(ts_code);
    }

    pub fn invalidate_many<I: IntoIterator<Item = TsCode>>(&self, codes: I) {
        let mut g = self.inner.write().expect("snapshot cache poisoned");
        for c in codes {
            g.remove(&c);
        }
    }

    /// 仅清除 quote.category 与传入 expected 不一致的项；用于 universe rebuild 后修复。
    pub fn invalidate_if_category_changed(
        &self,
        expected: &std::collections::HashMap<TsCode, crate::domain::shared::InstrumentCategory>,
    ) -> usize {
        let mut g = self.inner.write().expect("snapshot cache poisoned");
        let mut removed = 0;
        let stale: Vec<TsCode> = g
            .iter()
            .filter_map(|(k, v)| {
                expected
                    .get(k)
                    .copied()
                    .filter(|cat| *cat != v.quote.category)
                    .map(|_| k.clone())
            })
            .collect();
        for k in stale {
            g.remove(&k);
            removed += 1;
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::quotes::{QuoteSource, TradeStatus};
    use crate::domain::shared::{FreshnessStatus, InstrumentCategory};
    use chrono::Utc;

    fn quote(ts: &str, cat: InstrumentCategory) -> StockQuote {
        let code = TsCode::parse(ts).unwrap();
        let now = Utc::now();
        StockQuote {
            ts_code: code,
            name: None,
            category: cat,
            trade_date: TradeDate::parse("20260526").unwrap(),
            price: None,
            previous_close: None,
            open: None,
            high: None,
            low: None,
            change: None,
            change_percent: None,
            volume: None,
            amount: None,
            turnover_rate: None,
            volume_ratio: None,
            limit_up: None,
            limit_down: None,
            bid: Vec::new(),
            ask: Vec::new(),
            trade_status: TradeStatus::Unknown,
            source: QuoteSource::Tdx,
            captured_at: now,
            exchange_time: None,
            freshness: crate::domain::shared::Freshness {
                status: FreshnessStatus::Fresh,
                captured_at: Some(now),
                exchange_time: None,
                age_ms: Some(0),
                source: Some("tdx".to_string()),
                warning: None,
            },
            warnings: Vec::new(),
        }
    }

    #[test]
    fn put_then_get_roundtrips() {
        let c = SnapshotCache::new();
        let q = quote("600519.SH", InstrumentCategory::Stock);
        c.put(CachedSnapshot {
            quote: q.clone(),
            captured_at: q.captured_at,
            trade_date: q.trade_date,
            source: "tdx".into(),
        });
        let got = c.get(&q.ts_code).unwrap();
        assert_eq!(got.quote.category, InstrumentCategory::Stock);
    }

    #[test]
    fn invalidate_removes_entry() {
        let c = SnapshotCache::new();
        let q = quote("600519.SH", InstrumentCategory::Stock);
        c.put(CachedSnapshot {
            quote: q.clone(),
            captured_at: q.captured_at,
            trade_date: q.trade_date,
            source: "tdx".into(),
        });
        c.invalidate(&q.ts_code);
        assert!(c.get(&q.ts_code).is_none());
    }

    #[test]
    fn invalidate_if_category_changed_drops_mismatched() {
        let c = SnapshotCache::new();
        let q = quote("600519.SH", InstrumentCategory::Stock);
        c.put(CachedSnapshot {
            quote: q.clone(),
            captured_at: q.captured_at,
            trade_date: q.trade_date,
            source: "tdx".into(),
        });
        let mut expected = std::collections::HashMap::new();
        // 现在 universe 把该标的判定为 Index → 应被清掉
        expected.insert(q.ts_code.clone(), InstrumentCategory::Index);
        let removed = c.invalidate_if_category_changed(&expected);
        assert_eq!(removed, 1);
        assert!(c.get(&q.ts_code).is_none());
    }
}
