//! 账户快照重算 — cash / market value / unrealized PnL / freshness。
//!
//! Spec: docs/design/account-module.md §2 (账户估值和仓位价格计算)

use crate::domain::account::types::{AccountSnapshot, Position, PositionStatus};
use crate::domain::quotes::{MarketQuoteSnapshot, QuoteFacadeError, QuoteFacadeErrorKind};
use crate::domain::shared::{
    Freshness, FreshnessStatus, Money, OccurredAt, TsCode, WarningCode,
};
use crate::infrastructure::account::AccountRepository;
use crate::pipeline::account::quote_gateway::AccountQuoteGateway;
use chrono::Utc;
use rust_decimal::Decimal;
use std::collections::HashMap;

pub struct SnapshotBuildInput<'a> {
    pub repo: &'a AccountRepository<'a>,
    pub gateway: &'a dyn AccountQuoteGateway,
    pub now: OccurredAt,
}

#[derive(Debug, Clone)]
pub struct SnapshotResult {
    pub snapshot: AccountSnapshot,
    pub open_positions: Vec<Position>,
}

/// 重算 AccountSnapshot 派生字段。
///
/// Spec: account-module.md §2 字段说明 / §6 验收。
pub fn rebuild_snapshot(input: SnapshotBuildInput<'_>) -> rusqlite::Result<SnapshotResult> {
    let meta = input
        .repo
        .get_meta()?
        .expect("snapshot rebuild requires initialized account_meta");
    let frozen_cash = input.repo.total_frozen_cash()?;
    let cash = meta.cash;
    let available_cash = Money(cash.0 - frozen_cash.0);

    // 列出所有 open positions（带 lots / protection 等派生）
    let mut positions = input
        .repo
        .list_positions(Some(PositionStatus::Open), 10_000, 0)?;

    // 计算 sellableQuantity（lots 派生）
    let market_ctx = crate::domain::shared::resolve_market_time(input.now);
    let sellability_date = market_ctx
        .current_trade_date
        .unwrap_or(market_ctx.latest_completed_trade_date);
    for p in positions.iter_mut() {
        let lots = input.repo.list_lots_by_position(&p.position_id)?;
        let sellable: i64 = lots
            .iter()
            .filter(|l| l.sellable_from.as_naive() <= sellability_date.as_naive())
            .map(|l| (l.remaining_quantity.0 - l.frozen_quantity.0).max(0))
            .sum();
        p.sellable_quantity = crate::domain::shared::Shares(sellable);
        // 附加 protection
        p.protection = input.repo.get_protection(&p.position_id)?;
    }

    // 批量取 quote（去重）
    let codes: Vec<TsCode> = {
        let mut set: HashMap<String, TsCode> = HashMap::new();
        for p in &positions {
            set.insert(p.ts_code.as_str().into(), p.ts_code.clone());
        }
        set.into_values().collect()
    };
    let quote_results: HashMap<String, Result<MarketQuoteSnapshot, QuoteFacadeError>> = codes
        .iter()
        .map(|c| (c.as_str().to_string(), input.gateway.get_snapshot(c)))
        .collect();

    let mut priced = 0u32;
    let mut unpriced = 0u32;
    let mut total_market_value = Decimal::ZERO;
    let mut total_unrealized = Decimal::ZERO;
    let mut total_realized = Decimal::ZERO;
    let mut weakest_status = FreshnessStatus::Fresh;
    let mut any_valued = false;

    for p in positions.iter_mut() {
        total_realized += p.realized_pnl.0;
        let q = quote_results.get(p.ts_code.as_str());
        match q {
            Some(Ok(snap)) => {
                if let Some(price) = snap.quote.price {
                    let mv = price.0 * Decimal::from(p.quantity.0);
                    let unreal = (price.0 - p.avg_cost.0) * Decimal::from(p.quantity.0);
                    p.market_price = Some(price);
                    p.market_value = Some(Money(mv));
                    p.unrealized_pnl = Some(Money(unreal));
                    p.quote_freshness = Some(snap.quote.freshness.clone());
                    total_market_value += mv;
                    total_unrealized += unreal;
                    priced += 1;
                    any_valued = true;
                    weakest_status = weaken_status(weakest_status, snap.quote.freshness.status);
                } else {
                    // quote 存在但 price 缺失 → unpriced
                    p.quote_freshness = Some(snap.quote.freshness.clone());
                    p.warnings.push(WarningCode::QuotePriceMissing);
                    unpriced += 1;
                    weakest_status = FreshnessStatus::Stale; // 用 stale 表达部分估值
                }
            }
            Some(Err(e)) => {
                let warn = match e.kind {
                    QuoteFacadeErrorKind::QuoteMissing => WarningCode::QuoteMissing,
                    QuoteFacadeErrorKind::QuoteStale => WarningCode::QuoteStale,
                    QuoteFacadeErrorKind::QuotePriceMissing => WarningCode::QuotePriceMissing,
                    QuoteFacadeErrorKind::NotFound => WarningCode::InstrumentMissing,
                    QuoteFacadeErrorKind::DepthMissing => WarningCode::DepthMissing,
                    _ => WarningCode::QuoteMissing,
                };
                p.warnings.push(warn);
                p.quote_freshness = Some(Freshness {
                    status: FreshnessStatus::Missing,
                    captured_at: None,
                    exchange_time: None,
                    age_ms: None,
                    source: None,
                    warning: Some(warn),
                });
                unpriced += 1;
            }
            None => {
                unpriced += 1;
            }
        }
    }

    let valuation_status = if !any_valued && positions.is_empty() {
        // 无 open position
        FreshnessStatus::Missing
    } else if !any_valued {
        FreshnessStatus::Missing
    } else if unpriced > 0 {
        FreshnessStatus::Stale
    } else {
        weakest_status
    };

    let mut warnings: Vec<WarningCode> = Vec::new();
    if unpriced > 0 {
        warnings.push(WarningCode::DataPartial);
    }
    if matches!(valuation_status, FreshnessStatus::Stale) && !warnings.contains(&WarningCode::QuoteStale) {
        // 已经有 data_partial 时不冗余
    }

    let total_assets = cash.0 + total_market_value;
    let total_pnl = total_realized + total_unrealized;

    let snapshot = AccountSnapshot {
        initial_cash: meta.initial_cash,
        cash,
        available_cash,
        frozen_cash,
        market_value: Money(total_market_value),
        total_assets: Money(total_assets),
        realized_pnl: Money(total_realized),
        unrealized_pnl: Money(total_unrealized),
        total_pnl: Money(total_pnl),
        priced_position_count: priced,
        unpriced_position_count: unpriced,
        valuation_freshness: Freshness {
            status: valuation_status,
            captured_at: Some(input.now),
            exchange_time: None,
            age_ms: None,
            source: None,
            warning: None,
        },
        open_position_count: priced + unpriced,
        pending_order_count: input.repo.count_pending_orders()?,
        captured_at: input.now,
        warnings,
    };
    Ok(SnapshotResult {
        snapshot,
        open_positions: positions,
    })
}

fn weaken_status(a: FreshnessStatus, b: FreshnessStatus) -> FreshnessStatus {
    fn rank(s: FreshnessStatus) -> u8 {
        match s {
            FreshnessStatus::Fresh => 0,
            FreshnessStatus::Stale => 1,
            FreshnessStatus::Missing => 2,
        }
    }
    if rank(a) >= rank(b) {
        a
    } else {
        b
    }
}

/// 空账户的 snapshot（initial_cash 不可用时不应调用此函数）。
pub fn empty_snapshot(initial_cash: Money) -> AccountSnapshot {
    AccountSnapshot {
        initial_cash,
        cash: initial_cash,
        available_cash: initial_cash,
        frozen_cash: Money(Decimal::ZERO),
        market_value: Money(Decimal::ZERO),
        total_assets: initial_cash,
        realized_pnl: Money(Decimal::ZERO),
        unrealized_pnl: Money(Decimal::ZERO),
        total_pnl: Money(Decimal::ZERO),
        priced_position_count: 0,
        unpriced_position_count: 0,
        valuation_freshness: Freshness {
            status: FreshnessStatus::Missing,
            captured_at: Some(Utc::now()),
            exchange_time: None,
            age_ms: None,
            source: None,
            warning: None,
        },
        open_position_count: 0,
        pending_order_count: 0,
        captured_at: Utc::now(),
        warnings: vec![],
    }
}
