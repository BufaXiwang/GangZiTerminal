//! 账户金钱计算 — 佣金 / 印花税 / 平均成本 / 已实现盈亏。
//!
//! Spec: docs/design/account-module.md §2 (账户估值和仓位价格计算)
//!
//! 设计：金额类计算统一使用 `rust_decimal::Decimal` 避免 f64 精度坑。

use crate::domain::account::policy::AccountFeePolicy;
use crate::domain::shared::{Money, Price, Shares};
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;

/// 通用 Decimal 工具（保留 4 位精度后再 round 到 2 位"分"）。
pub struct MoneyMath;

impl MoneyMath {
    /// 成交金额 = price * quantity（保留 4 位精度）。
    pub fn gross_amount(price: Price, quantity: Shares) -> Decimal {
        price.0 * Decimal::from(quantity.0)
    }

    /// 加（saturating-style，Decimal 不溢出）。
    pub fn add(a: Money, b: Money) -> Money {
        Money(a.0 + b.0)
    }

    /// 减。
    pub fn sub(a: Money, b: Money) -> Money {
        Money(a.0 - b.0)
    }
}

/// 计算佣金（双向收取）。
///
/// Spec: account-module.md §2 成交模型 / §5 费用
/// `commission = max(gross * rate, min_commission)`，保留 2 位分。
pub fn compute_commission(
    price: Price,
    quantity: Shares,
    policy: &AccountFeePolicy,
) -> Money {
    let gross = MoneyMath::gross_amount(price, quantity);
    let rate = Decimal::from_f64(policy.commission_rate).unwrap_or(Decimal::ZERO);
    let raw = gross * rate;
    let raw_rounded = raw.round_dp(2);
    let min = policy.min_commission.0;
    let value = if raw_rounded < min { min } else { raw_rounded };
    Money(value)
}

/// 计算印花税（仅卖出）。
///
/// Spec: account-module.md §2 成交模型 / §5 费用
pub fn compute_stamp_tax(price: Price, quantity: Shares, policy: &AccountFeePolicy) -> Money {
    let gross = MoneyMath::gross_amount(price, quantity);
    let rate = Decimal::from_f64(policy.stamp_tax_sell_rate).unwrap_or(Decimal::ZERO);
    let raw = gross * rate;
    Money(raw.round_dp(2))
}

/// 买入成交后更新平均成本。
///
/// Spec: account-module.md §2 仓位模型规则:
/// `avgCost` 由买入成交价、买入佣金和剩余持仓数量加权派生。
///
/// 算法：cost_basis_new = old_qty * old_avg + buy_qty * buy_price + commission
///       avg_cost_new = cost_basis_new / (old_qty + buy_qty)
pub fn apply_buy_avg_cost(
    old_quantity: Shares,
    old_avg_cost: Price,
    buy_quantity: Shares,
    buy_price: Price,
    buy_commission: Money,
) -> Price {
    let old_qty = Decimal::from(old_quantity.0);
    let buy_qty = Decimal::from(buy_quantity.0);
    let total_qty = old_qty + buy_qty;
    if total_qty <= Decimal::ZERO {
        return Price(Decimal::ZERO);
    }
    let old_basis = old_qty * old_avg_cost.0;
    let new_buy_basis = buy_qty * buy_price.0 + buy_commission.0;
    let total = old_basis + new_buy_basis;
    Price((total / total_qty).round_dp(6))
}

/// 卖出成交后增量更新已实现盈亏。
///
/// Spec: account-module.md §2 仓位模型规则:
/// `realizedPnl` 由卖出成交收入减去被卖出 lot 的成本、佣金和印花税派生。
///
/// 这里使用平均成本法（avg_cost 已经包含买入佣金的加权）：
/// realized_pnl_delta = (sell_price - avg_cost) * sell_qty - sell_commission - stamp_tax
pub fn apply_sell_realized_pnl(
    sell_quantity: Shares,
    sell_price: Price,
    avg_cost: Price,
    sell_commission: Money,
    stamp_tax: Money,
) -> Money {
    let sell_qty = Decimal::from(sell_quantity.0);
    let revenue = sell_qty * sell_price.0;
    let cost = sell_qty * avg_cost.0;
    let pnl = (revenue - cost) - sell_commission.0 - stamp_tax.0;
    Money(pnl.round_dp(2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shared::{Money, Price, Shares};

    fn fee_policy() -> AccountFeePolicy {
        AccountFeePolicy::default()
    }

    #[test]
    fn commission_at_default_rate_above_min() {
        // 100 元 * 10000 股 = 1,000,000；0.0003 = 300 元 > 5 min
        let c = compute_commission(Price(Decimal::new(100, 0)), Shares(10_000), &fee_policy());
        assert_eq!(c, Money(Decimal::new(30000, 2)));
    }

    #[test]
    fn commission_hits_minimum_when_small() {
        // 1 元 * 100 股 = 100；0.0003 = 0.03，触发 min 5 元
        let c = compute_commission(Price(Decimal::new(1, 0)), Shares(100), &fee_policy());
        assert_eq!(c, Money(Decimal::new(500, 2)));
    }

    #[test]
    fn stamp_tax_at_default_rate() {
        // 100 * 10000 = 1,000,000；0.0005 = 500 元
        let s = compute_stamp_tax(Price(Decimal::new(100, 0)), Shares(10_000), &fee_policy());
        assert_eq!(s, Money(Decimal::new(50000, 2)));
    }

    #[test]
    fn stamp_tax_small_amount() {
        let s = compute_stamp_tax(Price(Decimal::new(1, 0)), Shares(100), &fee_policy());
        // 100 * 0.0005 = 0.05
        assert_eq!(s, Money(Decimal::new(5, 2)));
    }

    #[test]
    fn avg_cost_first_buy_includes_commission() {
        // 第一次买入：100 元 * 1000 股 + 30 元佣金 → 100,030 / 1000 = 100.03
        let avg = apply_buy_avg_cost(
            Shares(0),
            Price(Decimal::ZERO),
            Shares(1000),
            Price(Decimal::new(100, 0)),
            Money(Decimal::new(3000, 2)),
        );
        assert_eq!(avg, Price(Decimal::new(10003, 2)));
    }

    #[test]
    fn avg_cost_second_buy_weighted() {
        // 已持 1000 股 avg 100；再买 1000 股 110 + 33 元佣金
        // (1000*100 + 1000*110 + 33) / 2000 = 210033 / 2000 = 105.0165
        let avg = apply_buy_avg_cost(
            Shares(1000),
            Price(Decimal::new(100, 0)),
            Shares(1000),
            Price(Decimal::new(110, 0)),
            Money(Decimal::new(3300, 2)),
        );
        assert_eq!(avg, Price(Decimal::new(1050165, 4)));
    }

    #[test]
    fn realized_pnl_profit() {
        // avg 100，卖 110 * 1000 - 33 佣金 - 55 税 = (110-100)*1000 - 88 = 9912
        let pnl = apply_sell_realized_pnl(
            Shares(1000),
            Price(Decimal::new(110, 0)),
            Price(Decimal::new(100, 0)),
            Money(Decimal::new(3300, 2)),
            Money(Decimal::new(5500, 2)),
        );
        assert_eq!(pnl, Money(Decimal::new(991200, 2)));
    }

    #[test]
    fn realized_pnl_loss() {
        // avg 100，卖 90 * 1000 - 27 佣金 - 45 税 = -10000 - 72 = -10072
        let pnl = apply_sell_realized_pnl(
            Shares(1000),
            Price(Decimal::new(90, 0)),
            Price(Decimal::new(100, 0)),
            Money(Decimal::new(2700, 2)),
            Money(Decimal::new(4500, 2)),
        );
        assert_eq!(pnl, Money(Decimal::new(-1007200, 2)));
    }
}
