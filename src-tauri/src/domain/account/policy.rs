//! 费用策略 + 风控阈值。
//!
//! Spec: docs/design/account-module.md §2 硬风控模型

use crate::domain::shared::Money;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use specta::Type;

/// Spec: account-module.md §2 硬风控模型 — AccountFeePolicy
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AccountFeePolicy {
    /// 佣金率（双向收取）。
    pub commission_rate: f64,
    /// 最低佣金。
    pub min_commission: Money,
    /// 印花税率（仅卖出）。
    pub stamp_tax_sell_rate: f64,
    /// 过户费率（可选；默认 0）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transfer_fee_rate: Option<f64>,
}

/// Spec: account-module.md §2 缺省费用参数
///   `transferFeeRate = 0.00001`（A 股沪市过户费 0.001%，双向；SZ/BJ 不收）。
pub const FEE_DEFAULT: FeeDefaults = FeeDefaults {
    commission_rate: 0.0003,
    min_commission_cents: 500, // 5 元
    stamp_tax_sell_rate: 0.0005,
    transfer_fee_rate: 0.00001,
};

pub struct FeeDefaults {
    pub commission_rate: f64,
    pub min_commission_cents: i64,
    pub stamp_tax_sell_rate: f64,
    pub transfer_fee_rate: f64,
}

impl FeeDefaults {
    pub fn to_policy(&self) -> AccountFeePolicy {
        AccountFeePolicy {
            commission_rate: self.commission_rate,
            min_commission: Money(Decimal::new(self.min_commission_cents, 2)),
            stamp_tax_sell_rate: self.stamp_tax_sell_rate,
            transfer_fee_rate: Some(self.transfer_fee_rate),
        }
    }
}

impl Default for AccountFeePolicy {
    fn default() -> Self {
        FEE_DEFAULT.to_policy()
    }
}

/// Spec: account-module.md §2 硬风控模型 — AccountRiskPolicy
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AccountRiskPolicy {
    pub max_single_position_ratio: f64,
    pub max_gross_exposure_ratio: f64,
    pub max_order_value_ratio: f64,
    pub max_daily_new_orders: u32,
}

/// Spec: account-module.md §2 缺省风控阈值
pub const RISK_DEFAULT: AccountRiskPolicy = AccountRiskPolicy {
    max_single_position_ratio: 0.25,
    max_gross_exposure_ratio: 0.95,
    max_order_value_ratio: 0.25,
    max_daily_new_orders: 20,
};

impl Default for AccountRiskPolicy {
    fn default() -> Self {
        RISK_DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_default_matches_spec() {
        let p = AccountFeePolicy::default();
        assert!((p.commission_rate - 0.0003).abs() < 1e-12);
        assert_eq!(p.min_commission, Money(Decimal::new(500, 2)));
        assert!((p.stamp_tax_sell_rate - 0.0005).abs() < 1e-12);
        // Spec: account-module.md §2 — 缺省 transferFeeRate = 0.00001
        assert_eq!(p.transfer_fee_rate, Some(0.00001));
    }

    #[test]
    fn risk_default_matches_spec() {
        let r = AccountRiskPolicy::default();
        assert!((r.max_single_position_ratio - 0.25).abs() < 1e-12);
        assert!((r.max_gross_exposure_ratio - 0.95).abs() < 1e-12);
        assert!((r.max_order_value_ratio - 0.25).abs() < 1e-12);
        assert_eq!(r.max_daily_new_orders, 20);
    }
}
