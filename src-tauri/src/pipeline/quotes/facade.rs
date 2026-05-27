//! Snapshot facade — Quotes BC 给 Account / Agent 的同步 Rust 入口。
//!
//! Spec: docs/design/quotes-module.md §4 内部 Rust API
//!
//! Account 估值、成交模拟和保护条件评估必须走该 facade，不得使用 list_market 的摘要字段。
//!
//! 行为：
//! - 优先从 `SnapshotCache` 读 quote；缺失时尝试 close_snapshot（非交易时段）。
//! - 应用 quote 有效性规则（eligible trade date + 硬过期）。
//! - 派生 limitUp / limitDown / tradeStatus。
//! - 返回 `MarketQuoteSnapshot`，或者按 spec 返回错误（QuoteMissing / QuoteStale / ...）。

use crate::domain::quotes::{
    apply_band_helper, compute_limit_band, derive_freshness, eligible_trade_date, FreshnessIntent,
    MarketQuoteSnapshot, QuoteFacadeError, QuoteFacadeErrorKind,
};
use crate::domain::shared::{resolve_market_time, FreshnessStatus, TsCode};
use crate::infrastructure::db::AppDb;
use crate::infrastructure::quotes::{QuotesRepository, SnapshotCache};
use chrono::Utc;
use std::sync::Arc;

/// 同步读取一个标的的可用 quote snapshot。
///
/// 调用方需要：
/// - `db`: 持久化 handle（用于 fallback 到 close_snapshot）。
/// - `cache`: 进程内 snapshot cache。
/// - `ts_code`: 标的。
///
/// Spec: quotes-module.md §4 内部 Rust API
pub fn get_quote_snapshot(
    db: &AppDb,
    cache: &Arc<SnapshotCache>,
    ts_code: &TsCode,
) -> Result<MarketQuoteSnapshot, QuoteFacadeError> {
    let now = Utc::now();
    let ctx = resolve_market_time(now);
    let eligible = eligible_trade_date(&ctx);
    let repo = QuotesRepository::new(db);

    let inst = repo
        .get_instrument(ts_code)
        .map_err(|e| QuoteFacadeError::with_message(QuoteFacadeErrorKind::DbError, e.to_string()))?
        .ok_or_else(|| QuoteFacadeError::new(QuoteFacadeErrorKind::NotFound))?;

    let mut quote = match cache.get(ts_code) {
        Some(c) => c.quote,
        None => {
            if eligible.is_intraday {
                return Err(QuoteFacadeError::new(QuoteFacadeErrorKind::QuoteMissing));
            }
            repo.load_close_snapshot(ts_code, eligible.trade_date)
                .map_err(|e| {
                    QuoteFacadeError::with_message(QuoteFacadeErrorKind::DbError, e.to_string())
                })?
                .ok_or_else(|| QuoteFacadeError::new(QuoteFacadeErrorKind::QuoteMissing))?
        }
    };

    let source = quote.source.as_str().to_string();
    let (freshness, eligibility) = derive_freshness(
        &ctx,
        FreshnessIntent::Detail,
        quote.trade_date,
        quote.captured_at,
        &source,
    );
    if let Some(_w) = eligibility {
        return Err(QuoteFacadeError::new(QuoteFacadeErrorKind::QuoteMissing));
    }
    if matches!(freshness.status, FreshnessStatus::Stale) {
        return Err(QuoteFacadeError::new(QuoteFacadeErrorKind::QuoteStale));
    }
    if quote.price.is_none() || quote.previous_close.is_none() {
        return Err(QuoteFacadeError::new(
            QuoteFacadeErrorKind::QuotePriceMissing,
        ));
    }
    // 派生 limit band
    if let (Some(pc), Some(band)) = (
        quote.previous_close,
        compute_limit_band(
            &inst.ts_code,
            inst.category,
            inst.board.as_deref(),
            inst.is_st.unwrap_or(false),
        ),
    ) {
        if let Some((u, d)) = apply_band_helper(pc, band) {
            quote.limit_up = u;
            quote.limit_down = d;
        }
    }
    quote.freshness = freshness;
    let updated_at = quote.captured_at;
    Ok(MarketQuoteSnapshot {
        ts_code: ts_code.clone(),
        category: inst.category,
        quote,
        updated_at,
    })
}

/// 批量版本（per-item Result）。
pub fn get_quote_snapshots(
    db: &AppDb,
    cache: &Arc<SnapshotCache>,
    ts_codes: &[TsCode],
) -> Vec<Result<MarketQuoteSnapshot, QuoteFacadeError>> {
    ts_codes
        .iter()
        .map(|c| get_quote_snapshot(db, cache, c))
        .collect()
}

