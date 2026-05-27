//! 涨跌停价计算（纯规则）。
//!
//! Spec: docs/design/quotes-module.md §2 行情 DTO
//! - `limitUp` / `limitDown` 优先由 Quotes 基于 `previousClose`、`board`、`isSt`、上市日期 /
//!   公司事件和 A 股涨跌幅规则计算。
//! - 缺少必要输入时可为空，并必须返回 `quote_price_missing` warning。
//! - 计算必须使用纯规则；按最小价格 tick 舍入。

use crate::domain::shared::{InstrumentCategory, Market, Price, TsCode};
use rust_decimal::Decimal;
#[cfg(test)]
use rust_decimal::prelude::FromPrimitive;

/// Spec: quotes-module.md §2
///
/// 涨跌停带宽（百分点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitBand {
    pub up_percent: u32, // 例如 10 表示 10%
    pub down_percent: u32,
    /// 该标的当前是否有涨跌幅限制。
    pub bounded: bool,
}

/// 输入：标的、previousClose、是否 ST、board（"主板" / "创业板" / "科创板" / "北交所" 等）。
/// 缺少 previousClose 时调用方返回 quote_price_missing；该函数返回 None。
pub fn compute_limit_band(
    ts_code: &TsCode,
    category: InstrumentCategory,
    board: Option<&str>,
    is_st: bool,
) -> Option<LimitBand> {
    if !matches!(category, InstrumentCategory::Stock | InstrumentCategory::Fund) {
        // 指数无涨跌停。
        return Some(LimitBand {
            up_percent: 0,
            down_percent: 0,
            bounded: false,
        });
    }
    let market = ts_code.market();
    let code = &ts_code.as_str()[..6];
    // 创业板（300/301）、科创板（688/689）：20%
    // 北交所：30%
    // 主板（含 60x / 00x / 002）：10%；ST：5%
    let pct = if let Some(b) = board {
        match b {
            "科创板" | "STAR" => 20,
            "创业板" | "ChiNext" => 20,
            "北交所" | "BSE" => 30,
            _ => default_pct_by_code(market, code, is_st),
        }
    } else {
        default_pct_by_code(market, code, is_st)
    };
    Some(LimitBand {
        up_percent: pct,
        down_percent: pct,
        bounded: true,
    })
}

fn default_pct_by_code(market: Market, code6: &str, is_st: bool) -> u32 {
    match market {
        Market::BJ => 30,
        Market::SH => {
            if code6.starts_with("688") || code6.starts_with("689") {
                20
            } else if is_st {
                5
            } else {
                10
            }
        }
        Market::SZ => {
            if code6.starts_with("300") || code6.starts_with("301") {
                20
            } else if is_st {
                5
            } else {
                10
            }
        }
    }
}

/// 把 previousClose 按 `pct` 涨跌幅向上 / 向下计算涨跌停价，并按 tick 舍入。
///
/// A 股股票 tick = 0.01。返回 None 表示输入无效。
pub fn apply_band(previous_close: Price, band: LimitBand) -> Option<(Option<Price>, Option<Price>)> {
    if !band.bounded {
        return Some((None, None));
    }
    let pc = previous_close.0;
    if pc <= Decimal::ZERO {
        return None;
    }
    let up_factor = Decimal::ONE + Decimal::from(band.up_percent) / Decimal::from(100);
    let down_factor = Decimal::ONE - Decimal::from(band.down_percent) / Decimal::from(100);
    let up = (pc * up_factor).round_dp(2);
    let down = (pc * down_factor).round_dp(2);
    Some((Some(Price(up)), Some(Price(down))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shared::TsCode;

    #[test]
    fn sh_main_board_10pct() {
        let code = TsCode::parse("600519.SH").unwrap();
        let b = compute_limit_band(&code, InstrumentCategory::Stock, None, false).unwrap();
        assert_eq!(b.up_percent, 10);
    }

    #[test]
    fn star_market_20pct() {
        let code = TsCode::parse("688981.SH").unwrap();
        let b = compute_limit_band(&code, InstrumentCategory::Stock, None, false).unwrap();
        assert_eq!(b.up_percent, 20);
    }

    #[test]
    fn chinext_20pct() {
        let code = TsCode::parse("300750.SZ").unwrap();
        let b = compute_limit_band(&code, InstrumentCategory::Stock, None, false).unwrap();
        assert_eq!(b.up_percent, 20);
    }

    #[test]
    fn bj_30pct() {
        let code = TsCode::parse("430047.BJ").unwrap();
        let b = compute_limit_band(&code, InstrumentCategory::Stock, None, false).unwrap();
        assert_eq!(b.up_percent, 30);
    }

    #[test]
    fn st_5pct() {
        let code = TsCode::parse("600000.SH").unwrap();
        let b = compute_limit_band(&code, InstrumentCategory::Stock, None, true).unwrap();
        assert_eq!(b.up_percent, 5);
    }

    #[test]
    fn band_applies_with_round() {
        let pc = Price(Decimal::from_f64(10.0).unwrap());
        let (u, d) = apply_band(
            pc,
            LimitBand {
                up_percent: 10,
                down_percent: 10,
                bounded: true,
            },
        )
        .unwrap();
        assert_eq!(u.unwrap().0, Decimal::from_f64(11.0).unwrap().round_dp(2));
        assert_eq!(d.unwrap().0, Decimal::from_f64(9.0).unwrap().round_dp(2));
    }
}
