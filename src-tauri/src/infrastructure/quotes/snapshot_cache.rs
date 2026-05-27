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
}
