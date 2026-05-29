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
    fn get_snapshot(&self, ts_code: &TsCode) -> Result<MarketQuoteSnapshot, QuoteFacadeError> {
        match self.responses.lock().unwrap().get(ts_code.as_str()) {
            Some(Ok(snap)) => Ok(snap.clone()),
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
    fn get_display_snapshot(&self, ts_code: &TsCode) -> Option<MarketQuoteSnapshot> {
        // mock：复用 get_snapshot 的 Ok 分支作为可展示 quote。
        self.get_snapshot(ts_code).ok()
    }
}
