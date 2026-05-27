//! Account 交易规则不变量 — 整手 / 数量正 / 限价 / T+1。
//!
//! Spec: docs/design/account-module.md §2 (订单字段规则) / §5 (交易规则表)

use crate::domain::account::errors::{AccountError, AccountErrorKind};
use crate::domain::shared::{Price, Shares, TradeDate};
use rust_decimal::Decimal;

/// 股票 / 场内基金最小手数（spec §5：100 股 / 份）。
pub const LOT_SIZE: i64 = 100;

/// 校验数量必须为正且是 100 股 / 份整数倍。
///
/// Spec: account-module.md §2（quantity 字段规则）+ §5 (整手)
pub fn assert_lot_size(quantity: Shares) -> Result<(), AccountError> {
    validate_quantity_positive(quantity)?;
    if quantity.0 % LOT_SIZE != 0 {
        return Err(AccountError::with_message(
            AccountErrorKind::InvalidLotSize,
            format!("quantity {} not multiple of {}", quantity.0, LOT_SIZE),
        ));
    }
    Ok(())
}

/// 校验数量必须为正数。
pub fn validate_quantity_positive(quantity: Shares) -> Result<(), AccountError> {
    if quantity.0 <= 0 {
        return Err(AccountError::with_message(
            AccountErrorKind::InvalidInput,
            format!("quantity {} must be positive", quantity.0),
        ));
    }
    Ok(())
}

/// 校验 limit 订单价格 > 0。
///
/// Spec: account-module.md §4 operate_account 约束（`limit` 必须 limitPrice > 0）。
pub fn validate_limit_price(price: Option<Price>) -> Result<Price, AccountError> {
    let p = price.ok_or_else(|| {
        AccountError::with_message(AccountErrorKind::InvalidInput, "limit_price required")
    })?;
    if p.0 <= Decimal::ZERO {
        return Err(AccountError::with_message(
            AccountErrorKind::InvalidInput,
            format!("limit_price {} must be positive", p.0),
        ));
    }
    Ok(p)
}

/// 校验 lot 是否可卖（T+1）。
///
/// Spec: account-module.md §2 持仓批次模型 / §5 (T+1)
/// `sellable_from <= sellability_trade_date` 表示可卖。
pub fn is_lot_sellable(sellable_from: TradeDate, sellability_trade_date: TradeDate) -> bool {
    sellable_from.as_naive() <= sellability_trade_date.as_naive()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lot_size_100_passes() {
        assert!(assert_lot_size(Shares(100)).is_ok());
        assert!(assert_lot_size(Shares(500)).is_ok());
        assert!(assert_lot_size(Shares(1000)).is_ok());
    }

    #[test]
    fn lot_size_non_multiple_rejected() {
        let e = assert_lot_size(Shares(150)).unwrap_err();
        assert_eq!(e.kind, AccountErrorKind::InvalidLotSize);
    }

    #[test]
    fn lot_size_zero_rejected_as_invalid_input() {
        let e = assert_lot_size(Shares(0)).unwrap_err();
        assert_eq!(e.kind, AccountErrorKind::InvalidInput);
    }

    #[test]
    fn lot_size_negative_rejected_as_invalid_input() {
        let e = assert_lot_size(Shares(-100)).unwrap_err();
        assert_eq!(e.kind, AccountErrorKind::InvalidInput);
    }

    #[test]
    fn limit_price_zero_rejected() {
        let e = validate_limit_price(Some(Price(Decimal::ZERO))).unwrap_err();
        assert_eq!(e.kind, AccountErrorKind::InvalidInput);
    }

    #[test]
    fn limit_price_negative_rejected() {
        let e = validate_limit_price(Some(Price(Decimal::new(-1, 0)))).unwrap_err();
        assert_eq!(e.kind, AccountErrorKind::InvalidInput);
    }

    #[test]
    fn limit_price_required() {
        let e = validate_limit_price(None).unwrap_err();
        assert_eq!(e.kind, AccountErrorKind::InvalidInput);
    }

    #[test]
    fn limit_price_positive_passes() {
        let p = validate_limit_price(Some(Price(Decimal::new(10000, 2)))).unwrap();
        assert_eq!(p.0, Decimal::new(10000, 2));
    }

    #[test]
    fn t_plus_one_blocks_same_day_sell() {
        let buy_date = TradeDate::parse("20260526").unwrap();
        let next_day = TradeDate::parse("20260527").unwrap();
        // Lot bought on 20260526 with sellable_from = 20260527 cannot be sold on same day
        assert!(!is_lot_sellable(next_day, buy_date));
        // But can be sold on or after next trading day
        assert!(is_lot_sellable(next_day, next_day));
    }
}
