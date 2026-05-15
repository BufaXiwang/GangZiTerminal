//! A 股交易规则——纯函数，无 I/O。
//!
//! 涉及：T+1 / 整百 / 涨跌停 / 交易时段 / 资金 / 止损止盈合理性 / 手续费 / 印花税。

use super::errors::RuleError;
use super::types::Side;
use crate::domain::shared::{OccurredAt, Shares, Yuan};

// ============================================================================
// A 股成本常数
// ============================================================================

/// 整手大小——100 股。
pub const INTEGER_LOT_SIZE: i64 = 100;

/// 佣金费率——0.025%（双向）。
pub const COMMISSION_RATE: f64 = 0.00025;

/// 佣金最低收费——5 元（双向）。
pub const COMMISSION_MIN: f64 = 5.0;

/// 印花税费率——0.1%（仅卖出）。
pub const STAMP_TAX_RATE: f64 = 0.001;

/// 涨跌停容差——触板阈值留 0.5 个百分点（涨幅 9.95% 就算到顶）。
const LIMIT_NEAR_TOLERANCE: f64 = 0.005;

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

/// 涨跌停判定——主板 ±10%，创业板（300/301）+ 科创板（688）±20%，北交所（4/8/92）±30%。
///
/// **北交所 92xxx 新段（2023+）也走 ±30%**——之前漏判 920469 这种新代码段会被
/// 误归到主板 ±10%，在 20% 价位卡死。
pub fn price_limit_pct(code: &str) -> f64 {
    if code.starts_with('4') || code.starts_with('8') || code.starts_with("92") {
        0.30
    } else if code.starts_with("688") || code.starts_with("300") || code.starts_with("301") {
        0.20
    } else {
        0.10
    }
}

/// 校验当前价没触涨跌停（与交易方向同向时拒）。
///
/// - `change_percent` 是百分数（涨 1% → 1.0）；None 表示拿不到数据，放行
/// - Buy 触涨停拒，Sell 触跌停拒（反向交易允许——比如涨停可以卖）
pub fn ensure_price_not_limit(
    change_percent: Option<f64>,
    code: &str,
    side: Side,
) -> Result<(), RuleError> {
    let Some(pct_raw) = change_percent else {
        return Ok(());
    };
    let pct = pct_raw / 100.0; // 百分数 → 小数
    let limit = price_limit_pct(code);
    let near_limit = limit - LIMIT_NEAR_TOLERANCE;
    match side {
        Side::Buy if pct >= near_limit => Err(RuleError::PriceLimitHit {
            side: "涨",
            current_pct: pct * 100.0,
            limit_pct: limit * 100.0,
        }),
        Side::Sell if pct <= -near_limit => Err(RuleError::PriceLimitHit {
            side: "跌",
            current_pct: pct * 100.0,
            limit_pct: limit * 100.0,
        }),
        _ => Ok(()),
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
    fn price_limit_pct_buckets() {
        assert_eq!(price_limit_pct("600519"), 0.10); // 沪主板
        assert_eq!(price_limit_pct("000001"), 0.10); // 深主板
        assert_eq!(price_limit_pct("300750"), 0.20); // 创业板
        assert_eq!(price_limit_pct("301236"), 0.20); // 创业板 301 段
        assert_eq!(price_limit_pct("688981"), 0.20); // 科创板
        assert_eq!(price_limit_pct("430564"), 0.30); // 北交所老段 4xxxxx
        assert_eq!(price_limit_pct("832149"), 0.30); // 北交所老段 8xxxxx
        assert_eq!(price_limit_pct("920469"), 0.30); // 北交所新段 92xxxx
    }

    #[test]
    fn price_limit_blocks_buy_when_near_up_cap() {
        let q = Some(9.96); // 9.96% > 9.5% (主板 10% - 0.5% 容差)
        assert!(ensure_price_not_limit(q, "600519", Side::Buy).is_err());
    }

    #[test]
    fn price_limit_allows_sell_when_only_up_limit_hit() {
        let q = Some(9.96);
        assert!(ensure_price_not_limit(q, "600519", Side::Sell).is_ok());
    }

    #[test]
    fn price_limit_handles_chinext_20pct() {
        let q1 = Some(18.0);
        assert!(ensure_price_not_limit(q1, "300750", Side::Buy).is_ok()); // 还没到 19.5%
        let q2 = Some(19.6);
        assert!(ensure_price_not_limit(q2, "300750", Side::Buy).is_err()); // 触板
    }

    #[test]
    fn price_limit_handles_bj_30pct() {
        let q = Some(29.6);
        assert!(ensure_price_not_limit(q, "920469", Side::Buy).is_err()); // 北交所新段 +30%
        let q2 = Some(28.0);
        assert!(ensure_price_not_limit(q2, "920469", Side::Buy).is_ok());
    }

    #[test]
    fn price_limit_no_data_passes() {
        assert!(ensure_price_not_limit(None, "600519", Side::Buy).is_ok());
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
        // 100 股 × 1 元 = 100 元 × 0.025% = 0.025 元 → 触底线 5 元
        let fee = commission(Yuan::new(1.0).unwrap(), Shares::new(100).unwrap());
        assert_eq!(fee.value(), 5.0);
    }

    #[test]
    fn commission_proportional_when_large() {
        // 1000 股 × 100 元 = 100000 元 × 0.025% = 25 元
        let fee = commission(Yuan::new(100.0).unwrap(), Shares::new(1000).unwrap());
        assert!((fee.value() - 25.0).abs() < 1e-6);
    }

    #[test]
    fn stamp_tax_only_on_value() {
        // 1000 股 × 50 元 = 50000 元 × 0.1% = 50 元
        let tax = stamp_tax(Yuan::new(50.0).unwrap(), Shares::new(1000).unwrap());
        assert!((tax.value() - 50.0).abs() < 1e-6);
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
    fn a_share_code_validation() {
        assert!(ensure_a_share_code("600519").is_ok());
        assert!(ensure_a_share_code("000001").is_ok());
        assert!(ensure_a_share_code("920469").is_ok());
        assert!(ensure_a_share_code("60051").is_err()); // 5 位
        assert!(ensure_a_share_code("6005199").is_err()); // 7 位
        assert!(ensure_a_share_code("60051a").is_err()); // 含字母
    }
}
