//! 订单成交模拟 — market / limit。
//!
//! Spec: docs/design/account-module.md §5 订单成交模拟

use crate::domain::account::types::OrderSide;
use crate::domain::quotes::{MarketQuoteSnapshot, TradeStatus};
use crate::domain::shared::{FreshnessStatus, Price, Shares, Volume};
use rust_decimal::Decimal;

/// 价格 + 数量结果。`quantity` 可以小于请求数量（部分成交）。
#[derive(Debug, Clone, PartialEq)]
pub struct FillExecution {
    pub price: Price,
    pub quantity: Shares,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FillDecision {
    Filled(FillExecution),
    PartiallyFilled(FillExecution),
    /// 价格条件不达或盘口不可成交 → 保持 pending（仅 limit 路径）。
    NotEligible(NotEligibleReason),
}

#[derive(Debug, Clone, PartialEq)]
pub enum NotEligibleReason {
    Halted,
    OutsideTradingSession,
    QuoteStale,
    QuoteMissing,
    QuotePriceMissing,
    DepthMissing,
    LimitUpDownBlocked,
    PriceNotMatched,
}

/// Market / market-like 即时成交：要求 fresh quote + 可成交盘口。
///
/// Spec: account-module.md §5：
/// - 买入优先 ask[0]，卖出优先 bid[0]；盘口缺失不得成交。
/// - tradeStatus halted → instrument_suspended。
/// - tradeStatus closed / 非交易时段 → outside_trading_session。
/// - stale / missing quote 不得即时成交。
/// - 涨停 + 卖盘不可成交 → limit_up_down_blocked。
/// - 盘口量不足允许部分成交。
pub fn simulate_immediate(
    snapshot: &MarketQuoteSnapshot,
    side: OrderSide,
    quantity: Shares,
    is_trading_time: bool,
) -> FillDecision {
    // freshness 先检查
    match snapshot.quote.freshness.status {
        FreshnessStatus::Fresh => {}
        FreshnessStatus::Stale => return FillDecision::NotEligible(NotEligibleReason::QuoteStale),
        FreshnessStatus::Missing => return FillDecision::NotEligible(NotEligibleReason::QuoteMissing),
    }
    // trade status
    match snapshot.quote.trade_status {
        TradeStatus::Halted => return FillDecision::NotEligible(NotEligibleReason::Halted),
        TradeStatus::Closed => return FillDecision::NotEligible(NotEligibleReason::OutsideTradingSession),
        TradeStatus::Trading | TradeStatus::Unknown => {
            if !is_trading_time {
                return FillDecision::NotEligible(NotEligibleReason::OutsideTradingSession);
            }
        }
    }

    let depth = match side {
        OrderSide::Buy => &snapshot.quote.ask,
        OrderSide::Sell => &snapshot.quote.bid,
    };
    if depth.is_empty() {
        return FillDecision::NotEligible(NotEligibleReason::DepthMissing);
    }
    let top = &depth[0];
    let exec_price = match top.price {
        Some(p) => p,
        None => return FillDecision::NotEligible(NotEligibleReason::DepthMissing),
    };
    let top_volume = top.volume.unwrap_or(Volume(i64::MAX));
    // 涨跌停 + 盘口 0 量
    if let Some(limit_up) = snapshot.quote.limit_up {
        if matches!(side, OrderSide::Buy) && exec_price.0 >= limit_up.0 && top_volume.0 <= 0 {
            return FillDecision::NotEligible(NotEligibleReason::LimitUpDownBlocked);
        }
    }
    if let Some(limit_down) = snapshot.quote.limit_down {
        if matches!(side, OrderSide::Sell) && exec_price.0 <= limit_down.0 && top_volume.0 <= 0 {
            return FillDecision::NotEligible(NotEligibleReason::LimitUpDownBlocked);
        }
    }
    if top_volume.0 <= 0 {
        return FillDecision::NotEligible(NotEligibleReason::DepthMissing);
    }

    if top_volume.0 >= quantity.0 {
        FillDecision::Filled(FillExecution {
            price: exec_price,
            quantity,
        })
    } else {
        FillDecision::PartiallyFilled(FillExecution {
            price: exec_price,
            quantity: Shares(top_volume.0),
        })
    }
}

/// Limit 订单成交评估。
///
/// Spec: account-module.md §5
/// - `limit` 买单在 fresh quote 的一档卖价 `ask[0].price <= limitPrice` 且卖盘可成交时成交。
/// - `limit` 卖单在 fresh quote 的一档买价 `bid[0].price >= limitPrice` 且买盘可成交时成交。
/// - stale / missing quote 不得触发成交。
pub fn simulate_limit(
    snapshot: &MarketQuoteSnapshot,
    side: OrderSide,
    limit_price: Price,
    remaining_quantity: Shares,
    is_trading_time: bool,
) -> FillDecision {
    match snapshot.quote.freshness.status {
        FreshnessStatus::Fresh => {}
        FreshnessStatus::Stale => return FillDecision::NotEligible(NotEligibleReason::QuoteStale),
        FreshnessStatus::Missing => return FillDecision::NotEligible(NotEligibleReason::QuoteMissing),
    }
    match snapshot.quote.trade_status {
        TradeStatus::Halted => return FillDecision::NotEligible(NotEligibleReason::Halted),
        TradeStatus::Closed => return FillDecision::NotEligible(NotEligibleReason::OutsideTradingSession),
        TradeStatus::Trading | TradeStatus::Unknown => {
            if !is_trading_time {
                return FillDecision::NotEligible(NotEligibleReason::OutsideTradingSession);
            }
        }
    }
    let depth = match side {
        OrderSide::Buy => &snapshot.quote.ask,
        OrderSide::Sell => &snapshot.quote.bid,
    };
    if depth.is_empty() {
        return FillDecision::NotEligible(NotEligibleReason::DepthMissing);
    }
    let top = &depth[0];
    let counter_price = match top.price {
        Some(p) => p,
        None => return FillDecision::NotEligible(NotEligibleReason::DepthMissing),
    };
    let price_matched = match side {
        OrderSide::Buy => counter_price.0 <= limit_price.0,
        OrderSide::Sell => counter_price.0 >= limit_price.0,
    };
    if !price_matched {
        return FillDecision::NotEligible(NotEligibleReason::PriceNotMatched);
    }
    let top_volume = top.volume.unwrap_or(Volume(i64::MAX));
    if top_volume.0 <= 0 {
        return FillDecision::NotEligible(NotEligibleReason::DepthMissing);
    }
    // 涨跌停限价单：盘口已经满足价格条件即视为可成交（spec §5 例外允许成交）。
    if top_volume.0 >= remaining_quantity.0 {
        FillDecision::Filled(FillExecution {
            price: counter_price,
            quantity: remaining_quantity,
        })
    } else {
        FillDecision::PartiallyFilled(FillExecution {
            price: counter_price,
            quantity: Shares(top_volume.0),
        })
    }
}

/// 计算 limit buy 订单冻结的现金上界：`limitPrice * remainingQuantity + estimatedFees`。
///
/// Spec: account-module.md §2 冻结和重建规则。
pub fn estimate_buy_frozen_cash(limit_price: Price, quantity: Shares, est_fee: Decimal) -> Decimal {
    limit_price.0 * Decimal::from(quantity.0) + est_fee
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::quotes::{QuoteDepthLevel, QuoteSource, StockQuote, TradeStatus};
    use crate::domain::shared::{
        Freshness, FreshnessStatus, InstrumentCategory, Price, TradeDate, TsCode,
    };
    use chrono::Utc;
    use rust_decimal::Decimal;

    fn make_snapshot(
        bid: Vec<(f64, i64)>,
        ask: Vec<(f64, i64)>,
        trade_status: TradeStatus,
        freshness: FreshnessStatus,
    ) -> MarketQuoteSnapshot {
        let ts = TsCode::parse("600519.SH").unwrap();
        let to_levels = |v: Vec<(f64, i64)>| -> Vec<QuoteDepthLevel> {
            v.into_iter()
                .map(|(p, q)| QuoteDepthLevel {
                    price: Some(Price(Decimal::from_str_exact(&p.to_string()).unwrap())),
                    volume: Some(Volume(q)),
                })
                .collect()
        };
        let now = Utc::now();
        MarketQuoteSnapshot {
            ts_code: ts.clone(),
            category: InstrumentCategory::Stock,
            quote: StockQuote {
                ts_code: ts,
                name: None,
                category: InstrumentCategory::Stock,
                trade_date: TradeDate::parse("20260526").unwrap(),
                price: Some(Price(Decimal::new(100, 0))),
                previous_close: Some(Price(Decimal::new(99, 0))),
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
                bid: to_levels(bid),
                ask: to_levels(ask),
                trade_status,
                source: QuoteSource::Tdx,
                captured_at: now,
                exchange_time: None,
                freshness: Freshness {
                    status: freshness,
                    captured_at: Some(now),
                    exchange_time: None,
                    age_ms: None,
                    source: Some("tdx".into()),
                    warning: None,
                },
                warnings: vec![],
            },
            updated_at: now,
        }
    }

    #[test]
    fn market_buy_fills_at_ask_when_volume_enough() {
        let s = make_snapshot(
            vec![(99.0, 10_000)],
            vec![(100.0, 10_000)],
            TradeStatus::Trading,
            FreshnessStatus::Fresh,
        );
        let r = simulate_immediate(&s, OrderSide::Buy, Shares(1000), true);
        match r {
            FillDecision::Filled(f) => {
                assert_eq!(f.price.0, Decimal::new(100, 0));
                assert_eq!(f.quantity.0, 1000);
            }
            _ => panic!("expected filled"),
        }
    }

    #[test]
    fn market_buy_partial_when_volume_short() {
        let s = make_snapshot(
            vec![(99.0, 10_000)],
            vec![(100.0, 300)],
            TradeStatus::Trading,
            FreshnessStatus::Fresh,
        );
        let r = simulate_immediate(&s, OrderSide::Buy, Shares(1000), true);
        match r {
            FillDecision::PartiallyFilled(f) => {
                assert_eq!(f.quantity.0, 300);
            }
            _ => panic!("expected partial"),
        }
    }

    #[test]
    fn market_buy_blocked_when_stale() {
        let s = make_snapshot(
            vec![(99.0, 10_000)],
            vec![(100.0, 10_000)],
            TradeStatus::Trading,
            FreshnessStatus::Stale,
        );
        let r = simulate_immediate(&s, OrderSide::Buy, Shares(1000), true);
        assert!(matches!(
            r,
            FillDecision::NotEligible(NotEligibleReason::QuoteStale)
        ));
    }

    #[test]
    fn market_buy_blocked_when_halted() {
        let s = make_snapshot(
            vec![(99.0, 10_000)],
            vec![(100.0, 10_000)],
            TradeStatus::Halted,
            FreshnessStatus::Fresh,
        );
        let r = simulate_immediate(&s, OrderSide::Buy, Shares(1000), true);
        assert!(matches!(
            r,
            FillDecision::NotEligible(NotEligibleReason::Halted)
        ));
    }

    #[test]
    fn market_buy_blocked_when_no_depth() {
        let s = make_snapshot(
            vec![(99.0, 10_000)],
            vec![],
            TradeStatus::Trading,
            FreshnessStatus::Fresh,
        );
        let r = simulate_immediate(&s, OrderSide::Buy, Shares(1000), true);
        assert!(matches!(
            r,
            FillDecision::NotEligible(NotEligibleReason::DepthMissing)
        ));
    }

    #[test]
    fn market_buy_blocked_outside_trading_time() {
        let s = make_snapshot(
            vec![(99.0, 10_000)],
            vec![(100.0, 10_000)],
            TradeStatus::Trading,
            FreshnessStatus::Fresh,
        );
        let r = simulate_immediate(&s, OrderSide::Buy, Shares(1000), false);
        assert!(matches!(
            r,
            FillDecision::NotEligible(NotEligibleReason::OutsideTradingSession)
        ));
    }

    #[test]
    fn limit_buy_fills_when_ask_le_limit() {
        let s = make_snapshot(
            vec![(99.0, 10_000)],
            vec![(99.5, 10_000)],
            TradeStatus::Trading,
            FreshnessStatus::Fresh,
        );
        let r = simulate_limit(
            &s,
            OrderSide::Buy,
            Price(Decimal::new(100, 0)),
            Shares(1000),
            true,
        );
        match r {
            FillDecision::Filled(f) => assert_eq!(f.price.0, Decimal::new(995, 1)),
            _ => panic!(),
        }
    }

    #[test]
    fn limit_buy_no_match_when_ask_above_limit() {
        let s = make_snapshot(
            vec![(99.0, 10_000)],
            vec![(101.0, 10_000)],
            TradeStatus::Trading,
            FreshnessStatus::Fresh,
        );
        let r = simulate_limit(
            &s,
            OrderSide::Buy,
            Price(Decimal::new(100, 0)),
            Shares(1000),
            true,
        );
        assert!(matches!(
            r,
            FillDecision::NotEligible(NotEligibleReason::PriceNotMatched)
        ));
    }

    #[test]
    fn limit_sell_fills_when_bid_ge_limit() {
        let s = make_snapshot(
            vec![(100.5, 10_000)],
            vec![(101.0, 10_000)],
            TradeStatus::Trading,
            FreshnessStatus::Fresh,
        );
        let r = simulate_limit(
            &s,
            OrderSide::Sell,
            Price(Decimal::new(100, 0)),
            Shares(500),
            true,
        );
        match r {
            FillDecision::Filled(f) => {
                assert_eq!(f.price.0, Decimal::new(1005, 1));
                assert_eq!(f.quantity.0, 500);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn limit_sell_blocked_when_stale() {
        let s = make_snapshot(
            vec![(100.5, 10_000)],
            vec![(101.0, 10_000)],
            TradeStatus::Trading,
            FreshnessStatus::Stale,
        );
        let r = simulate_limit(
            &s,
            OrderSide::Sell,
            Price(Decimal::new(100, 0)),
            Shares(500),
            true,
        );
        assert!(matches!(
            r,
            FillDecision::NotEligible(NotEligibleReason::QuoteStale)
        ));
    }

    #[test]
    fn estimate_frozen_cash_includes_fees() {
        // 100 * 1000 + 30 = 100030
        let f = estimate_buy_frozen_cash(Price(Decimal::new(100, 0)), Shares(1000), Decimal::new(30, 0));
        assert_eq!(f, Decimal::new(100030, 0));
    }
}
