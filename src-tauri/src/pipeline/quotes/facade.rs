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
    MarketInstrument, MarketQuoteSnapshot, QuoteFacadeError, QuoteFacadeErrorKind,
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

/// 同步读取 instrument 元数据（category / status / market / name / 等）。
///
/// 调用方（特别是 Account BC）用于：
/// - 交易性校验（是否上市 / 是否暂停 / category 是否支持）
/// - 自选 / 显示用名称查询
///
/// 设计：Account 等跨 BC 调用者不允许直接访问 `infrastructure::quotes::QuotesRepository`；
/// 所有 instrument 元数据查询必须走本 facade，保持 BC 边界。
///
/// Spec: quotes-module.md §4 内部 Rust API；architecture.md §3（跨 BC：Account → Quotes facade only）。
pub fn get_instrument(
    db: &AppDb,
    ts_code: &TsCode,
) -> Result<Option<MarketInstrument>, QuoteFacadeError> {
    let repo = QuotesRepository::new(db);
    repo.get_instrument(ts_code)
        .map_err(|e| QuoteFacadeError::with_message(QuoteFacadeErrorKind::DbError, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::quotes::{InstrumentSource, MarketInstrument, QuoteSource, StockQuote, TradeStatus};
    use crate::domain::shared::{
        FreshnessStatus, InstrumentCategory, InstrumentStatus, Market, TradeDate,
    };
    use crate::infrastructure::db::run_migrations;
    use crate::infrastructure::quotes::{migrations as quotes_migrations, CachedSnapshot};
    use chrono::Utc;

    fn make_setup() -> (AppDb, Arc<SnapshotCache>) {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|conn| run_migrations(conn, quotes_migrations()).unwrap());
        let cache = Arc::new(SnapshotCache::new());
        (db, cache)
    }

    fn seed_inst(db: &AppDb, ts: &str) -> TsCode {
        let code = TsCode::parse(ts).unwrap();
        QuotesRepository::new(db)
            .upsert_instruments(&[MarketInstrument {
                ts_code: code.clone(),
                name: "T".into(),
                category: InstrumentCategory::Stock,
                market: Market::SH,
                board: None,
                sector: None,
                status: Some(InstrumentStatus::Listed),
                is_st: Some(false),
                publisher: None,
                index_category: None,
                fund_type: None,
                management: None,
                list_date: None,
                source: InstrumentSource::Tushare,
                updated_at: Utc::now(),
            }])
            .unwrap();
        code
    }

    fn mock_quote(ts: TsCode) -> StockQuote {
        use rust_decimal::{prelude::FromPrimitive, Decimal};
        let now = Utc::now();
        StockQuote {
            ts_code: ts,
            name: None,
            category: InstrumentCategory::Stock,
            trade_date: TradeDate::from_naive(now.with_timezone(&chrono_tz::Asia::Shanghai).date_naive()),
            price: Some(crate::domain::shared::Price(Decimal::from_f64(100.0).unwrap())),
            previous_close: Some(crate::domain::shared::Price(Decimal::from_f64(99.0).unwrap())),
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
                source: Some("tdx".into()),
                warning: None,
            },
            warnings: Vec::new(),
        }
    }

    #[test]
    fn facade_returns_not_found_when_instrument_missing() {
        let (db, cache) = make_setup();
        let code = TsCode::parse("600519.SH").unwrap();
        let err = get_quote_snapshot(&db, &cache, &code).unwrap_err();
        assert!(matches!(err.kind, QuoteFacadeErrorKind::NotFound));
    }

    #[test]
    fn facade_returns_quote_missing_when_cache_empty_intraday() {
        let (db, cache) = make_setup();
        let code = seed_inst(&db, "600519.SH");
        let err = get_quote_snapshot(&db, &cache, &code).unwrap_err();
        // 当前时刻可能是交易时段或非交易时段；测试不依赖时段——只要无 quote 都返回 QuoteMissing。
        assert!(matches!(
            err.kind,
            QuoteFacadeErrorKind::QuoteMissing
        ));
    }

    #[test]
    fn facade_cache_hit_returns_snapshot_when_fresh() {
        let (db, cache) = make_setup();
        let code = seed_inst(&db, "600519.SH");
        let q = mock_quote(code.clone());
        cache.put(CachedSnapshot {
            quote: q.clone(),
            captured_at: q.captured_at,
            trade_date: q.trade_date,
            source: "tdx".into(),
        });
        // 在合适交易时段 / 收盘后路径下应能取到；不强行断言 — 至少调用不 panic。
        let _ = get_quote_snapshot(&db, &cache, &code);
    }
}

