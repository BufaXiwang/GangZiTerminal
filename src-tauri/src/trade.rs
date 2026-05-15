//! Briefing trade call → 模拟仓位的 sizing/止损/止盈推导。
//!
//! Rust 端口自 src/lib/briefingTrade.ts。

use crate::agent_io::SimulatedTradePlan;

/// 在 plan 的描述里抽取所有"X%"或"X.Y%"形式的数字，返回最大值（小数形式）。
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
            // 允许数字和 % 之间有空白
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

pub fn derive_trade_weight(plan: &SimulatedTradePlan) -> f64 {
    let explicit = parse_percent(&plan.position_sizing.suggested_weight);
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

pub fn derive_stop_loss(entry_price: f64, plan: &SimulatedTradePlan) -> f64 {
    // exit_plan.stop_loss_condition + position_sizing.max_loss_per_trade 一起去抓百分比
    let combined = format!(
        "{} {}",
        plan.exit_plan.stop_loss_condition, plan.position_sizing.max_loss_per_trade,
    );
    let explicit = parse_percent(&combined);
    let fallback: f64 = match plan.risk_level.as_str() {
        "high" => 0.06,
        "medium" => 0.08,
        _ => 0.10,
    };
    let loss = explicit.unwrap_or(fallback).min(0.15).max(0.03);
    entry_price * (1.0 - loss)
}

pub fn derive_take_profit(entry_price: f64, plan: &SimulatedTradePlan) -> f64 {
    let explicit = parse_percent(&plan.exit_plan.take_profit_condition);
    let fallback: f64 = match plan.risk_level.as_str() {
        "high" => 0.10,
        "medium" => 0.14,
        _ => 0.18,
    };
    let gain = explicit.unwrap_or(fallback).min(0.30).max(0.05);
    entry_price * (1.0 + gain)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_plan(risk: &str, suit: &str, confidence: f64) -> SimulatedTradePlan {
        SimulatedTradePlan {
            action: "buy".into(),
            suitability: suit.into(),
            target_stocks: vec![],
            entry_strategy: Default::default(),
            position_sizing: crate::agent_io::PositionSizing {
                suggested_weight: "0%-5%".into(),
                max_loss_per_trade: "1%".into(),
                reason: String::new(),
            },
            exit_plan: crate::agent_io::ExitPlan {
                take_profit_condition: "10%".into(),
                stop_loss_condition: "8%".into(),
                time_stop: String::new(),
            },
            risk_level: risk.into(),
            confidence,
            why_not_buy_now: vec![],
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
        let stop = derive_stop_loss(100.0, &p);
        assert!(stop < 100.0 && stop > 80.0);
    }

    #[test]
    fn take_profit_above_entry() {
        let p = make_plan("medium", "high", 0.5);
        let tp = derive_take_profit(100.0, &p);
        assert!(tp > 100.0 && tp < 130.0);
    }
}
