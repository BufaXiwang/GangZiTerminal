//! 仓位 sizing——从 briefing trade plan 推导建议权重 / 止损 / 止盈 / 时间止损。
//!
//! 这些函数读账户层自己的轻量 sizing 输入（不带 prices），返回**比例**和**绝对价**。
//! Agent / briefing 的输出在 adapter 或 pipeline 层映射成 `SizingInput`，domain 不依赖
//! `agent_io`。
//!
//! 移自原 `crate::trade`——纯函数，无 I/O。

use crate::domain::shared::{OccurredAt, Yuan};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SizingInput {
    pub suggested_weight: String,
    pub max_loss_per_trade: String,
    pub stop_loss_condition: String,
    pub take_profit_condition: String,
    pub risk_level: String,
    pub suitability: String,
    pub confidence: f64,
}

/// 在 plan 的描述里抽取所有 "X%" / "X.Y%" 数字，返回最大值（小数形式）。
/// 手写避免引入 regex crate。
fn parse_percent(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    let mut max_val: Option<f64> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            let num_str = &text[start..i];
            while i < bytes.len() && bytes[i] == b' ' {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'%' {
                if let Ok(v) = num_str.parse::<f64>() {
                    let val = v / 100.0;
                    if val.is_finite() {
                        max_val = Some(max_val.map_or(val, |cur| cur.max(val)));
                    }
                }
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    max_val
}

/// 推导仓位权重——基于 plan 的 risk_level / suitability / confidence，封顶 1%-8%。
pub fn derive_trade_weight(plan: &SizingInput) -> f64 {
    let explicit = parse_percent(&plan.suggested_weight);
    let risk_cap: f64 = match plan.risk_level.as_str() {
        "high" => 0.03,
        "medium" => 0.05,
        _ => 0.08,
    };
    let suitability_cap = match plan.suitability.as_str() {
        "high" => risk_cap,
        "medium" => risk_cap.min(0.05),
        _ => 0.02,
    };
    let confidence_cap = (suitability_cap.min(plan.confidence * 0.08)).max(0.01);
    let candidate = explicit.unwrap_or(confidence_cap);
    candidate.min(confidence_cap).min(0.08).max(0.01)
}

/// 推导止损价——结合 plan 的 stop_loss_condition + max_loss_per_trade，封顶 3%-15%。
pub fn derive_stop_loss(entry_price: Yuan, plan: &SizingInput) -> Yuan {
    let combined = format!("{} {}", plan.stop_loss_condition, plan.max_loss_per_trade,);
    let explicit = parse_percent(&combined);
    let fallback: f64 = match plan.risk_level.as_str() {
        "high" => 0.06,
        "medium" => 0.08,
        _ => 0.10,
    };
    let loss = explicit.unwrap_or(fallback).min(0.15).max(0.03);
    Yuan::from_unchecked(entry_price.value() * (1.0 - loss))
}

/// 推导止盈价——基于 plan 的 take_profit_condition，封顶 5%-30%。
pub fn derive_take_profit(entry_price: Yuan, plan: &SizingInput) -> Yuan {
    let explicit = parse_percent(&plan.take_profit_condition);
    let fallback: f64 = match plan.risk_level.as_str() {
        "high" => 0.10,
        "medium" => 0.14,
        _ => 0.18,
    };
    let gain = explicit.unwrap_or(fallback).min(0.30).max(0.05);
    Yuan::from_unchecked(entry_price.value() * (1.0 + gain))
}

/// 推导时间止损绝对时间——entry_at 加 7 个日历日。
///
/// prompt 里 timeStop 的语义是"3-5 个交易日仍未验证则复盘退出"——A 股大约 5 个交易日
/// = 7 日历日。用日历日避免后端要查节假日表；过头一两天对训练目的影响微弱。
pub fn derive_time_stop_at(entered_at: OccurredAt) -> OccurredAt {
    OccurredAt::new(entered_at.value() + 7 * 24 * 3600 * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_plan(risk: &str, suit: &str, confidence: f64) -> SizingInput {
        SizingInput {
            suggested_weight: "0%-5%".into(),
            max_loss_per_trade: "1%".into(),
            take_profit_condition: "10%".into(),
            stop_loss_condition: "8%".into(),
            risk_level: risk.into(),
            suitability: suit.into(),
            confidence,
            ..Default::default()
        }
    }

    #[test]
    fn weight_caps_at_8pc() {
        let p = make_plan("low", "high", 1.0);
        assert!(derive_trade_weight(&p) <= 0.08);
    }

    #[test]
    fn stop_loss_below_entry() {
        let p = make_plan("medium", "high", 0.5);
        let stop = derive_stop_loss(Yuan::new(100.0).unwrap(), &p);
        assert!(stop.value() < 100.0 && stop.value() > 80.0);
    }

    #[test]
    fn take_profit_above_entry() {
        let p = make_plan("medium", "high", 0.5);
        let tp = derive_take_profit(Yuan::new(100.0).unwrap(), &p);
        assert!(tp.value() > 100.0 && tp.value() < 130.0);
    }

    #[test]
    fn time_stop_adds_seven_days() {
        let now = OccurredAt::new(1_000_000_000_000);
        let ts = derive_time_stop_at(now);
        let diff_ms = ts.value() - now.value();
        assert_eq!(diff_ms, 7 * 24 * 3600 * 1000);
    }
}
