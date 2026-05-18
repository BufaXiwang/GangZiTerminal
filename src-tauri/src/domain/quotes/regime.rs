//! 市场状态（Regime）—— bull/bear/choppy 三态。
//!
//! 用途：principle 召回时按 regime 过滤，避免牛市学的原则用到熊市。
//! Thesis 创建时也快照当时的 regime（regime_at_creation），方便复盘归因。
//!
//! 归属 quotes BC：regime 是市场结构判断，agent / account 都只读。
//!
//! 检测算法（在 infrastructure/quotes 里实现，本文件只放纯类型）：
//! - Bull: 上证指数 > 60 日均线 且 20 日均线 > 60 日均线
//! - Bear: 上证指数 < 60 日均线 且 20 日均线 < 60 日均线
//! - Choppy: 其他

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Regime {
    Bull,
    Bear,
    Choppy,
}

impl Regime {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bull => "bull",
            Self::Bear => "bear",
            Self::Choppy => "choppy",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "bull" => Some(Self::Bull),
            "bear" => Some(Self::Bear),
            "choppy" => Some(Self::Choppy),
            _ => None,
        }
    }
}

impl std::fmt::Display for Regime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 纯函数 regime 判定。
///
/// 输入：按时间升序的收盘价序列（最少 60 个点；不够 → Choppy 保守值）。
/// 规则：
/// - 当前价 > 60 日均线 且 MA20 > MA60 → Bull
/// - 当前价 < 60 日均线 且 MA20 < MA60 → Bear
/// - 否则 Choppy
pub fn detect_regime(closes: &[f64]) -> Regime {
    if closes.len() < 60 {
        return Regime::Choppy;
    }
    let len = closes.len();
    let current = closes[len - 1];
    let ma20: f64 = closes[len - 20..len].iter().sum::<f64>() / 20.0;
    let ma60: f64 = closes[len - 60..len].iter().sum::<f64>() / 60.0;
    if current > ma60 && ma20 > ma60 {
        Regime::Bull
    } else if current < ma60 && ma20 < ma60 {
        Regime::Bear
    } else {
        Regime::Choppy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bull_when_uptrend() {
        // 60 个递增价：current 远超 MA60
        let closes: Vec<f64> = (1..=60).map(|i| i as f64).collect();
        assert_eq!(detect_regime(&closes), Regime::Bull);
    }

    #[test]
    fn bear_when_downtrend() {
        let closes: Vec<f64> = (1..=60).rev().map(|i| i as f64).collect();
        assert_eq!(detect_regime(&closes), Regime::Bear);
    }

    #[test]
    fn choppy_when_flat() {
        let closes = vec![100.0; 60];
        assert_eq!(detect_regime(&closes), Regime::Choppy);
    }

    #[test]
    fn choppy_when_insufficient_data() {
        let closes: Vec<f64> = (1..=30).map(|i| i as f64).collect();
        assert_eq!(detect_regime(&closes), Regime::Choppy);
    }
}
