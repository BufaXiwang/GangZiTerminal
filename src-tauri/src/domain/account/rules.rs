//! A 股交易规则——纯函数，无 I/O。
//!
//! 涉及：T+1 / 整百 / 填单可行性（盘口）/ 交易时段 / 资金 / 止损止盈合理性 /
//! 手续费 / 印花税。
//!
//! **不判断"是否触及涨跌停"**——封板信号由盘口直接给：对手方一档为空 = 不可填单。
//! 这样我们不用硬编码 ±5/10/20/30 阈值，也不用维护 ST 名单 / 新股期 / 板块映射。

use super::errors::RuleError;
use crate::domain::shared::{Lots, OccurredAt, Shares, Yuan};

// ============================================================================
// A 股成本常数
// ============================================================================

/// 整手大小——100 股。
pub const INTEGER_LOT_SIZE: i64 = 100;

/// 佣金费率（双向）—— spec account-module.md §5 default：`commissionRate = 0.0003`。
pub const COMMISSION_RATE: f64 = 0.0003;

/// 佣金最低收费（双向）—— spec default：`minCommission = 5`。
pub const COMMISSION_MIN: f64 = 5.0;

/// 印花税费率（仅卖出）—— spec default：`stampTaxSellRate = 0.0005`。
pub const STAMP_TAX_RATE: f64 = 0.0005;

/// 过户费率（spec default：`transferFeeRate = 0`；保留接口，沪市可后续启用）。
pub const TRANSFER_FEE_RATE: f64 = 0.0;

// ============================================================================
// AccountRiskPolicy —— spec account-module.md §2/§5。
// ============================================================================

/// 单票仓位上限（市值 / riskEquity）—— spec default 25%。
pub const MAX_SINGLE_POSITION_RATIO: f64 = 0.25;
/// 总仓位上限（总市值 + 在途买单冻结 / riskEquity）—— spec default 95%。
pub const MAX_GROSS_EXPOSURE_RATIO: f64 = 0.95;
/// 单笔订单金额上限 / riskEquity —— spec default 25%。
pub const MAX_ORDER_VALUE_RATIO: f64 = 0.25;
/// 每自然日 actor=agent 新建订单数上限 —— spec default 20。
pub const MAX_DAILY_NEW_ORDERS: u32 = 20;

#[derive(Debug, Clone, Copy)]
pub struct AccountRiskPolicy {
    pub max_single_position_ratio: f64,
    pub max_gross_exposure_ratio: f64,
    pub max_order_value_ratio: f64,
    pub max_daily_new_orders: u32,
}

impl Default for AccountRiskPolicy {
    fn default() -> Self {
        Self {
            max_single_position_ratio: MAX_SINGLE_POSITION_RATIO,
            max_gross_exposure_ratio: MAX_GROSS_EXPOSURE_RATIO,
            max_order_value_ratio: MAX_ORDER_VALUE_RATIO,
            max_daily_new_orders: MAX_DAILY_NEW_ORDERS,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AccountFeePolicy {
    pub commission_rate: f64,
    pub min_commission: f64,
    pub stamp_tax_sell_rate: f64,
    pub transfer_fee_rate: f64,
}

impl Default for AccountFeePolicy {
    fn default() -> Self {
        Self {
            commission_rate: COMMISSION_RATE,
            min_commission: COMMISSION_MIN,
            stamp_tax_sell_rate: STAMP_TAX_RATE,
            transfer_fee_rate: TRANSFER_FEE_RATE,
        }
    }
}

// ============================================================================
// 校验函数
// ============================================================================

/// 校验 6 位 A 股代码。
pub fn ensure_a_share_code(code: &str) -> Result<(), RuleError> {
    if code.len() == 6 && code.chars().all(|c| c.is_ascii_digit()) {
        Ok(())
    } else {
        Err(RuleError::InvalidCode(code.to_string()))
    }
}

/// 校验整百股（开仓 / 加仓 / 减仓 delta 必须 ≥100 且 100 倍数）。
pub fn ensure_integer_lot(shares: i64) -> Result<(), RuleError> {
    if shares >= INTEGER_LOT_SIZE && shares % INTEGER_LOT_SIZE == 0 {
        Ok(())
    } else {
        Err(RuleError::SharesNotIntegerLot { shares })
    }
}

/// 校验当前在 A 股交易时段。
pub fn ensure_trading_hours() -> Result<(), RuleError> {
    if crate::domain::quotes::is_a_share_trading_hours() {
        Ok(())
    } else {
        Err(RuleError::OutsideTradingHours)
    }
}

/// 校验 T+1：不能当日买当日卖。
pub fn ensure_t_plus_one(entry_at: OccurredAt) -> Result<(), RuleError> {
    let entry_day = beijing_date_str(entry_at);
    let today = beijing_date_str(OccurredAt::now());
    if entry_day == today {
        Err(RuleError::TPlusOneViolation {
            entry_date: entry_day,
            today,
        })
    } else {
        Ok(())
    }
}

fn beijing_date_str(ts: OccurredAt) -> String {
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ts.value())
        .unwrap_or_else(chrono::Utc::now)
        + chrono::Duration::hours(8);
    dt.format("%Y-%m-%d").to_string()
}

/// 校验对手方盘口有量——订单能填掉。
///
/// `counterparty_top` 是对手方一档量：
/// - 买入时传**卖一量**（`ask_levels[0].volume`）
/// - 卖出时传**买一量**（`bid_levels[0].volume`）
///
/// 这是 A 股封板 / 停牌 / 数据降级的统一信号：盘口空 → 拒交易，不用区分原因。
/// **没有 fallback**：拿不到盘口（fallback 源、未订阅 code）等同没法填——直接拒，
/// 避免 agent 学到"封板照样能成交"的假经验。
pub fn ensure_fillable(counterparty_top: Option<Lots>) -> Result<(), RuleError> {
    match counterparty_top {
        Some(v) if v.value() > 0 => Ok(()),
        _ => Err(RuleError::NotFillable),
    }
}

/// 校验止损止盈数值合理（>0 且与当前价的方向关系正确）。
pub fn ensure_stops_make_sense(
    price: Yuan,
    stop_loss: Option<Yuan>,
    take_profit: Option<Yuan>,
) -> Result<(), RuleError> {
    let p = price.value();
    if let Some(sl) = stop_loss {
        let v = sl.value();
        if !v.is_finite() || v <= 0.0 {
            return Err(RuleError::InvalidStops(format!("止损价 {v} 非法")));
        }
        if v >= p {
            return Err(RuleError::InvalidStops(format!(
                "止损 {v:.2} 必须低于当前价 {p:.2}"
            )));
        }
    }
    if let Some(tp) = take_profit {
        let v = tp.value();
        if !v.is_finite() || v <= 0.0 {
            return Err(RuleError::InvalidStops(format!("止盈价 {v} 非法")));
        }
        if v <= p {
            return Err(RuleError::InvalidStops(format!(
                "止盈 {v:.2} 必须高于当前价 {p:.2}"
            )));
        }
    }
    Ok(())
}

// ============================================================================
// 成本计算
// ============================================================================

/// 计算佣金（0.025%，最低 5 元）。开仓 / 加仓 / 减仓 / 平仓**都收**。
pub fn commission(price: Yuan, shares: Shares) -> Yuan {
    let trade_value = price.value() * shares.value() as f64;
    let fee = (trade_value * COMMISSION_RATE).max(COMMISSION_MIN);
    Yuan::from_unchecked(fee)
}

/// 计算印花税（0.1%）。**仅卖出**——开仓 / 加仓无印花税。
pub fn stamp_tax(price: Yuan, shares: Shares) -> Yuan {
    let trade_value = price.value() * shares.value() as f64;
    Yuan::from_unchecked(trade_value * STAMP_TAX_RATE)
}

/// 加仓后的新均价 = (旧均价 × 旧股数 + 新价 × 加仓股数) / 新总股数。
pub fn compute_new_avg_price(
    old_avg: Yuan,
    old_shares: Shares,
    new_price: Yuan,
    delta_shares: Shares,
) -> Yuan {
    let total_shares = old_shares.value() + delta_shares.value();
    if total_shares <= 0 {
        return old_avg;
    }
    let new_avg = (old_avg.value() * old_shares.value() as f64
        + new_price.value() * delta_shares.value() as f64)
        / total_shares as f64;
    Yuan::from_unchecked(new_avg)
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_lot_rule() {
        assert!(ensure_integer_lot(100).is_ok());
        assert!(ensure_integer_lot(200).is_ok());
        assert!(ensure_integer_lot(1500).is_ok());
        assert!(ensure_integer_lot(50).is_err());
        assert!(ensure_integer_lot(150).is_err());
        assert!(ensure_integer_lot(0).is_err());
    }

    #[test]
    fn fillable_when_counterparty_has_volume() {
        assert!(ensure_fillable(Some(Lots::from_unchecked(100))).is_ok());
    }

    #[test]
    fn not_fillable_when_counterparty_empty() {
        // 封板 / 停牌典型场景：对手方 0 量
        assert!(ensure_fillable(Some(Lots::from_unchecked(0))).is_err());
    }

    #[test]
    fn not_fillable_when_orderbook_missing() {
        // fallback 源没有盘口，或股票未订阅——拒交易
        assert!(ensure_fillable(None).is_err());
    }

    #[test]
    fn stops_make_sense() {
        let price = Yuan::new(11.0).unwrap();
        assert!(ensure_stops_make_sense(price, Some(Yuan::new(10.0).unwrap()), None).is_ok());
        assert!(ensure_stops_make_sense(price, None, Some(Yuan::new(12.0).unwrap())).is_ok());
        // 止损 >= 当前价
        assert!(ensure_stops_make_sense(price, Some(Yuan::new(11.5).unwrap()), None).is_err());
        // 止盈 <= 当前价
        assert!(ensure_stops_make_sense(price, None, Some(Yuan::new(11.0).unwrap())).is_err());
        // 止损 ≤ 0
        assert!(ensure_stops_make_sense(price, Some(Yuan::new(-1.0).unwrap()), None).is_err());
    }

    #[test]
    fn commission_min_floor() {
        // 100 股 × 1 元 = 100 元 × 0.03% = 0.03 元 → 触底线 5 元
        let fee = commission(Yuan::new(1.0).unwrap(), Shares::new(100).unwrap());
        assert_eq!(fee.value(), 5.0);
    }

    #[test]
    fn commission_proportional_when_large() {
        // 1000 股 × 100 元 = 100000 元 × 0.03% = 30 元（spec default rate）
        let fee = commission(Yuan::new(100.0).unwrap(), Shares::new(1000).unwrap());
        assert!((fee.value() - 30.0).abs() < 1e-6);
    }

    #[test]
    fn stamp_tax_only_on_value() {
        // 1000 股 × 50 元 = 50000 元 × 0.05% = 25 元（spec default rate）
        let tax = stamp_tax(Yuan::new(50.0).unwrap(), Shares::new(1000).unwrap());
        assert!((tax.value() - 25.0).abs() < 1e-6);
    }

    #[test]
    fn new_avg_price_weighted() {
        // 100 股 @ 10 元 + 加仓 100 股 @ 12 元 → 均价 11 元
        let new_avg = compute_new_avg_price(
            Yuan::new(10.0).unwrap(),
            Shares::new(100).unwrap(),
            Yuan::new(12.0).unwrap(),
            Shares::new(100).unwrap(),
        );
        assert!((new_avg.value() - 11.0).abs() < 1e-6);
    }

    #[test]
    fn t_plus_one_blocks_same_day() {
        let now = OccurredAt::now();
        assert!(ensure_t_plus_one(now).is_err());
    }

    #[test]
    fn t_plus_one_allows_yesterday() {
        let yesterday = OccurredAt::new(OccurredAt::now().value() - 2 * 24 * 3600 * 1000);
        assert!(ensure_t_plus_one(yesterday).is_ok());
    }

    #[test]
    fn t_plus_one_uses_last_acquisition_not_entered_at() {
        // 回归：旧版 scale_out / close 用的是 target.entered_at——昨天开仓 + 今天加仓
        // 后会绕过 T+1（entered_at 看着昨天）。aggregate 现在传 last_acquisition_at；
        // 这里直接断言 rule 在两种时间点下的对比行为。
        let two_days_ago = OccurredAt::new(OccurredAt::now().value() - 2 * 24 * 3600 * 1000);
        let today = OccurredAt::now();
        // 用 entered_at（两天前）→ 旧逻辑放行
        assert!(ensure_t_plus_one(two_days_ago).is_ok());
        // 用 last_acquisition_at（今天）→ 新逻辑拒
        assert!(ensure_t_plus_one(today).is_err());
    }

    #[test]
    fn a_share_code_validation() {
        assert!(ensure_a_share_code("600519").is_ok());
        assert!(ensure_a_share_code("000001").is_ok());
        assert!(ensure_a_share_code("920469").is_ok());
        assert!(ensure_a_share_code("60051").is_err()); // 5 位
        assert!(ensure_a_share_code("6005199").is_err()); // 7 位
        assert!(ensure_a_share_code("60051a").is_err()); // 含字母
    }
}
