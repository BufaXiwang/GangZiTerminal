//! Account → Quotes 跨 BC 边界。只通过 `pipeline::quotes::facade` 读取行情。
//!
//! Spec: docs/design/architecture.md §3（Account → Quotes snapshot only）
//!
//! 设计：用 trait 抽象，让 service 测试可以注入 mock quote provider，
//! 同时生产路径通过 `QuotesFacadeGateway` 调用真实 facade。

#[cfg(test)]
use crate::domain::quotes::QuoteFacadeErrorKind;
use crate::domain::quotes::{MarketQuoteSnapshot, QuoteFacadeError};
use crate::domain::shared::TsCode;
use crate::infrastructure::db::AppDb;
use crate::infrastructure::quotes::SnapshotCache;
use crate::pipeline::quotes::facade as quotes_facade;
use std::sync::Arc;

/// Account 内部 quote gateway 抽象，用于隔离 Quotes facade 调用，方便单元测试 mock。
///
/// Spec: account-module.md §2 不变量（Account 写路径必须 fail closed on stale / missing quote）。
pub trait AccountQuoteGateway: Send + Sync {
    fn get_snapshot(&self, ts_code: &TsCode) -> Result<MarketQuoteSnapshot, QuoteFacadeError>;
    fn get_snapshots(
        &self,
        ts_codes: &[TsCode],
    ) -> Vec<Result<MarketQuoteSnapshot, QuoteFacadeError>>;

    /// 显示专用：返回可展示 quote（含 stale），无可用时 None。
    /// 仅用于读取展示（watchlist / 估值显示），**不**用于交易写路径。
    /// Spec: account-module.md §4 line 459。
    fn get_display_snapshot(&self, ts_code: &TsCode) -> Option<MarketQuoteSnapshot>;
}

/// 生产路径：调用 `pipeline::quotes::facade`。
pub struct QuotesFacadeGateway {
    db: AppDb,
    cache: Arc<SnapshotCache>,
}

impl QuotesFacadeGateway {
    pub fn new(db: AppDb, cache: Arc<SnapshotCache>) -> Self {
        Self { db, cache }
    }
}

impl AccountQuoteGateway for QuotesFacadeGateway {
    fn get_snapshot(&self, ts_code: &TsCode) -> Result<MarketQuoteSnapshot, QuoteFacadeError> {
        quotes_facade::get_quote_snapshot(&self.db, &self.cache, ts_code)
    }
    fn get_snapshots(
        &self,
        ts_codes: &[TsCode],
    ) -> Vec<Result<MarketQuoteSnapshot, QuoteFacadeError>> {
        quotes_facade::get_quote_snapshots(&self.db, &self.cache, ts_codes)
    }
    fn get_display_snapshot(&self, ts_code: &TsCode) -> Option<MarketQuoteSnapshot> {
        quotes_facade::get_quote_for_display(&self.db, &self.cache, ts_code)
    }
}

/// 单测专用 mock：用 map 模拟 quote 结果。
#[cfg(test)]
pub struct MockQuoteGateway {
    pub responses:
        std::sync::Mutex<std::collections::HashMap<String, Result<MarketQuoteSnapshot, QuoteFacadeErrorKind>>>,
}

#[cfg(test)]
impl MockQuoteGateway {
    pub fn new() -> Self {
        Self {
            responses: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn set(&self, ts_code: &TsCode, result: Result<MarketQuoteSnapshot, QuoteFacadeErrorKind>) {
        self.responses
            .lock()
            .unwrap()
            .insert(ts_code.as_str().to_string(), result);
    }
}

#[cfg(test)]
impl AccountQuoteGateway for MockQuoteGateway {
    /// Faithful 生产语义（fail-closed）：与 `QuotesFacadeGateway` / facade 一致——
    /// stale 快照 **返回 `Err(QuoteStale)`**（交易写路径不得用 stale 成交）。
    ///
    /// 测试仍用 `.set(code, Ok(snap))` 配置「底层 snapshot」，stale/fresh 由 snapshot
    /// 自身的 `freshness` 决定，mock 据此派生 Err/Ok，避免测试预设「反的」语义。
    /// Spec: facade.rs `get_quote_snapshot`（stale → Err）；account-module.md §4 line 459。
    fn get_snapshot(&self, ts_code: &TsCode) -> Result<MarketQuoteSnapshot, QuoteFacadeError> {
        use crate::domain::shared::FreshnessStatus;
        match self.responses.lock().unwrap().get(ts_code.as_str()) {
            Some(Ok(snap)) => match snap.quote.freshness.status {
                FreshnessStatus::Stale => Err(QuoteFacadeError::new(QuoteFacadeErrorKind::QuoteStale)),
                FreshnessStatus::Missing => {
                    Err(QuoteFacadeError::new(QuoteFacadeErrorKind::QuoteMissing))
                }
                FreshnessStatus::Fresh => Ok(snap.clone()),
            },
            Some(Err(kind)) => Err(QuoteFacadeError::new(*kind)),
            None => Err(QuoteFacadeError::new(QuoteFacadeErrorKind::QuoteMissing)),
        }
    }
    fn get_snapshots(
        &self,
        ts_codes: &[TsCode],
    ) -> Vec<Result<MarketQuoteSnapshot, QuoteFacadeError>> {
        ts_codes.iter().map(|c| self.get_snapshot(c)).collect()
    }
    /// Stale-tolerant 展示语义（与 facade `get_quote_for_display` 一致）：
    /// 返回底层 snapshot（fresh 或 stale）作为可展示 quote；无配置 / Err 配置 → None。
    fn get_display_snapshot(&self, ts_code: &TsCode) -> Option<MarketQuoteSnapshot> {
        match self.responses.lock().unwrap().get(ts_code.as_str()) {
            Some(Ok(snap)) => Some(snap.clone()),
            _ => None,
        }
    }
}
