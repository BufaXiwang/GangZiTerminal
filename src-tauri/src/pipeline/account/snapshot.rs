//! 账户快照重算 — cash / market value / unrealized PnL / freshness。
//!
//! Spec: docs/design/account-module.md §2 (账户估值和仓位价格计算)

use crate::domain::account::types::{AccountSnapshot, Position, PositionStatus};
use crate::domain::quotes::MarketQuoteSnapshot;
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
///
/// 当 `account_meta` 缺失（账户未初始化）时，返回空 SnapshotResult，
/// 让 caller 走 empty_snapshot 路径而不是 panic。
pub fn rebuild_snapshot(input: SnapshotBuildInput<'_>) -> rusqlite::Result<SnapshotResult> {
    let Some(meta) = input.repo.get_meta()? else {
        return Ok(SnapshotResult {
            snapshot: empty_snapshot(Money(Decimal::ZERO)),
            open_positions: Vec::new(),
        });
    };
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
    // 估值是**显示读取**：用 display 路径（stale 照常返回 + 跨日回落），不用交易级
    // fail-closed get_snapshot——否则 stale-but-priced 持仓会被当成 unpriced、整账
    // valuationFreshness 错误地降成 missing。交易写路径另走 fail-closed。
    // Spec account §4 line 459 + §2 valuationFreshness 聚合（stale 无 missing → stale）。
    let quote_results: HashMap<String, Option<MarketQuoteSnapshot>> = codes
        .iter()
        .map(|c| (c.as_str().to_string(), input.gateway.get_display_snapshot(c)))
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
            Some(Some(snap)) => {
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
                    // quote 存在但 price 缺失 → unpriced（无法估值，按 missing 子 quote 聚合）
                    p.quote_freshness = Some(snap.quote.freshness.clone());
                    p.warnings.push(WarningCode::QuotePriceMissing);
                    unpriced += 1;
                }
            }
            // display 路径无可用 quote（含完全无 quote / instrument 缺失）→ unpriced
            _ => {
                let warn = WarningCode::QuoteMissing;
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
        }
    }

    // Spec: account-module.md §2 账户快照 valuationFreshness:
    //   - openPositionCount = 0 → fresh（无仓位无估值需求）
    //   - 有仓位时按子 quote 聚合：全 fresh → fresh；存在 stale 且无 missing → stale；
    //     存在 missing 子 quote（含完全无可估值）→ missing。
    let valuation_status = if positions.is_empty() {
        FreshnessStatus::Fresh
    } else if !any_valued {
        FreshnessStatus::Missing
    } else if unpriced > 0 {
        // 部分仓位 missing → 整体 missing
        FreshnessStatus::Missing
    } else {
        weakest_status
    };

    let mut warnings: Vec<WarningCode> = Vec::new();
    if unpriced > 0 {
        warnings.push(WarningCode::DataPartial);
    }
    // 估值整体 stale（有 stale 子 quote、无 missing）→ 附 quote_stale，供 UI 提示
    // "估值基于过期行情"。Spec §2 valuationFreshness 聚合 + warnings。
    if matches!(valuation_status, FreshnessStatus::Stale)
        && !warnings.contains(&WarningCode::QuoteStale)
    {
        warnings.push(WarningCode::QuoteStale);
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

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    #[test]
    fn empty_snapshot_zero_positions_freshness_is_fresh() {
        // Spec: account-module.md §2 账户快照 — 0 仓位 → fresh
        let s = empty_snapshot(Money(Decimal::from(1_000_000)));
        assert_eq!(s.open_position_count, 0);
        assert_eq!(s.valuation_freshness.status, FreshnessStatus::Fresh);
        assert!(s.warnings.is_empty());
    }
}

/// 空账户的 snapshot（initial_cash 不可用时不应调用此函数）。
///
/// Spec: account-module.md §2 账户快照 — `openPositionCount = 0` 时 `status = "fresh"`。
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
            status: FreshnessStatus::Fresh,
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
