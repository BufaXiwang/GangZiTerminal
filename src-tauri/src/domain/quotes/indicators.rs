//! 技术指标——spec `quotes-module.md §2`。
//!
//! 固定 20 元 `IndicatorName` 枚举；新增指标必须先扩展本 enum 和 spec。
//! 所有公式按 spec 参数表：
//!
//! | IndicatorName | 参数 / 公式 |
//! |---|---|
//! | ma5 / ma10 / ma20 / ma60 | close 简单移动平均，窗口分别为 5 / 10 / 20 / 60 |
//! | ema12 / ema26 | close EMA，span 分别为 12 / 26 |
//! | macd_dif | ema12 − ema26 |
//! | macd_dea | macd_dif EMA，span = 9 |
//! | macd_hist | 2 × (macd_dif − macd_dea) |
//! | rsi6 / rsi12 / rsi24 | Wilder RSI，窗口分别为 6 / 12 / 24，基于 close 变化 |
//! | kdj_k / kdj_d / kdj_j | RSV window = 9，K smoothing = 3，D smoothing = 3，初始 K/D = 50，J = 3*K − 2*D |
//! | boll_mid | close MA20 |
//! | boll_upper / boll_lower | MA20 ± 2 × population_stddev(close, 20) |
//! | volume_ma5 / volume_ma10 | volume 简单移动平均，窗口分别为 5 / 10 |
//!
//! 全部 pure，无 I/O。窗口不足时对应值为 `None`，不能用 0 代替。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::types::KlinePoint;
use crate::domain::shared::{OccurredAt, TradeDate};

/// spec 固定 20 元枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndicatorName {
    Ma5,
    Ma10,
    Ma20,
    Ma60,
    Ema12,
    Ema26,
    MacdDif,
    MacdDea,
    MacdHist,
    Rsi6,
    Rsi12,
    Rsi24,
    KdjK,
    KdjD,
    KdjJ,
    BollUpper,
    BollMid,
    BollLower,
    VolumeMa5,
    VolumeMa10,
}

impl IndicatorName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ma5 => "ma5",
            Self::Ma10 => "ma10",
            Self::Ma20 => "ma20",
            Self::Ma60 => "ma60",
            Self::Ema12 => "ema12",
            Self::Ema26 => "ema26",
            Self::MacdDif => "macd_dif",
            Self::MacdDea => "macd_dea",
            Self::MacdHist => "macd_hist",
            Self::Rsi6 => "rsi6",
            Self::Rsi12 => "rsi12",
            Self::Rsi24 => "rsi24",
            Self::KdjK => "kdj_k",
            Self::KdjD => "kdj_d",
            Self::KdjJ => "kdj_j",
            Self::BollUpper => "boll_upper",
            Self::BollMid => "boll_mid",
            Self::BollLower => "boll_lower",
            Self::VolumeMa5 => "volume_ma5",
            Self::VolumeMa10 => "volume_ma10",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ma5" => Self::Ma5,
            "ma10" => Self::Ma10,
            "ma20" => Self::Ma20,
            "ma60" => Self::Ma60,
            "ema12" => Self::Ema12,
            "ema26" => Self::Ema26,
            "macd_dif" => Self::MacdDif,
            "macd_dea" => Self::MacdDea,
            "macd_hist" => Self::MacdHist,
            "rsi6" => Self::Rsi6,
            "rsi12" => Self::Rsi12,
            "rsi24" => Self::Rsi24,
            "kdj_k" => Self::KdjK,
            "kdj_d" => Self::KdjD,
            "kdj_j" => Self::KdjJ,
            "boll_upper" => Self::BollUpper,
            "boll_mid" => Self::BollMid,
            "boll_lower" => Self::BollLower,
            "volume_ma5" => Self::VolumeMa5,
            "volume_ma10" => Self::VolumeMa10,
            _ => return None,
        })
    }

    pub fn all() -> [Self; 20] {
        [
            Self::Ma5,
            Self::Ma10,
            Self::Ma20,
            Self::Ma60,
            Self::Ema12,
            Self::Ema26,
            Self::MacdDif,
            Self::MacdDea,
            Self::MacdHist,
            Self::Rsi6,
            Self::Rsi12,
            Self::Rsi24,
            Self::KdjK,
            Self::KdjD,
            Self::KdjJ,
            Self::BollUpper,
            Self::BollMid,
            Self::BollLower,
            Self::VolumeMa5,
            Self::VolumeMa10,
        ]
    }
}

/// `IndicatorSnapshot` —— spec §2。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndicatorSnapshot {
    pub ts_code: String,
    pub basis: IndicatorBasis,
    pub values: BTreeMap<IndicatorName, Option<f64>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndicatorBasis {
    /// "day" | "week" | "month"
    pub period: String,
    /// "none" | "qfq" | "hfq"
    pub adjust: String,
    pub fetched_at: OccurredAt,
}

// ============================================================================
// Computation —— pure functions over &[KlinePoint]
// ============================================================================

pub fn compute_indicators(
    ts_code: &str,
    klines: &[KlinePoint],
    basis: IndicatorBasis,
    selection: Option<&[IndicatorName]>,
) -> IndicatorSnapshot {
    let closes: Vec<f64> = klines.iter().map(|k| k.close.value()).collect();
    let highs: Vec<f64> = klines.iter().map(|k| k.high.value()).collect();
    let lows: Vec<f64> = klines.iter().map(|k| k.low.value()).collect();
    let vols: Vec<f64> = klines.iter().map(|k| k.volume.value() as f64).collect();

    let mut values: BTreeMap<IndicatorName, Option<f64>> = BTreeMap::new();
    let wanted: &[IndicatorName] = match selection {
        Some(s) => s,
        None => &IndicatorName::all(),
    };
    let ema12_series = ema_series(&closes, 12);
    let ema26_series = ema_series(&closes, 26);
    let dif_series: Vec<Option<f64>> = ema12_series
        .iter()
        .zip(ema26_series.iter())
        .map(|(a, b)| match (a, b) {
            (Some(x), Some(y)) => Some(x - y),
            _ => None,
        })
        .collect();
    let dif_vals: Vec<f64> = dif_series.iter().filter_map(|v| *v).collect();
    let dea_series = ema_series(&dif_vals, 9);

    let (kdj_k, kdj_d, kdj_j) = kdj_series(&closes, &highs, &lows, 9, 3, 3);

    for name in wanted {
        let v = match name {
            IndicatorName::Ma5 => ma_last(&closes, 5),
            IndicatorName::Ma10 => ma_last(&closes, 10),
            IndicatorName::Ma20 => ma_last(&closes, 20),
            IndicatorName::Ma60 => ma_last(&closes, 60),
            IndicatorName::Ema12 => ema12_series.last().copied().flatten(),
            IndicatorName::Ema26 => ema26_series.last().copied().flatten(),
            IndicatorName::MacdDif => dif_series.last().copied().flatten(),
            IndicatorName::MacdDea => dea_series.last().copied().flatten(),
            IndicatorName::MacdHist => match (dif_series.last(), dea_series.last()) {
                (Some(Some(dif)), Some(Some(dea))) => Some(2.0 * (dif - dea)),
                _ => None,
            },
            IndicatorName::Rsi6 => rsi_last(&closes, 6),
            IndicatorName::Rsi12 => rsi_last(&closes, 12),
            IndicatorName::Rsi24 => rsi_last(&closes, 24),
            IndicatorName::KdjK => kdj_k,
            IndicatorName::KdjD => kdj_d,
            IndicatorName::KdjJ => kdj_j,
            IndicatorName::BollMid => ma_last(&closes, 20),
            IndicatorName::BollUpper => boll(&closes, 20, true),
            IndicatorName::BollLower => boll(&closes, 20, false),
            IndicatorName::VolumeMa5 => ma_last(&vols, 5),
            IndicatorName::VolumeMa10 => ma_last(&vols, 10),
        };
        values.insert(*name, v);
    }

    IndicatorSnapshot {
        ts_code: ts_code.to_string(),
        basis,
        values,
        warnings: Vec::new(),
    }
}

fn ma_last(xs: &[f64], n: usize) -> Option<f64> {
    if xs.len() < n || n == 0 {
        return None;
    }
    let s: f64 = xs[xs.len() - n..].iter().sum();
    Some(s / n as f64)
}

/// EMA 系列；前 (n-1) 个返回 None；从 index = n-1 开始用 SMA 引导，之后用 α=2/(n+1)。
fn ema_series(xs: &[f64], n: usize) -> Vec<Option<f64>> {
    let mut out: Vec<Option<f64>> = Vec::with_capacity(xs.len());
    if n == 0 || xs.is_empty() {
        return out;
    }
    let alpha = 2.0 / (n as f64 + 1.0);
    let mut prev: Option<f64> = None;
    for (i, x) in xs.iter().enumerate() {
        if i + 1 < n {
            out.push(None);
            continue;
        }
        if i + 1 == n {
            let seed = xs[..n].iter().sum::<f64>() / n as f64;
            prev = Some(seed);
            out.push(Some(seed));
            continue;
        }
        let v = alpha * x + (1.0 - alpha) * prev.unwrap();
        prev = Some(v);
        out.push(Some(v));
    }
    out
}

/// Wilder RSI（spec：基于 close 变化的 Wilder 平滑）。窗口不足返 None。
fn rsi_last(closes: &[f64], n: usize) -> Option<f64> {
    if closes.len() <= n || n == 0 {
        return None;
    }
    // Wilder seed: average gain / loss over first n diffs
    let mut gains = 0.0f64;
    let mut losses = 0.0f64;
    for w in 1..=n {
        let d = closes[w] - closes[w - 1];
        if d >= 0.0 {
            gains += d;
        } else {
            losses += -d;
        }
    }
    let mut avg_gain = gains / n as f64;
    let mut avg_loss = losses / n as f64;
    // smooth remaining
    for i in n + 1..closes.len() {
        let d = closes[i] - closes[i - 1];
        let g = if d >= 0.0 { d } else { 0.0 };
        let l = if d < 0.0 { -d } else { 0.0 };
        avg_gain = (avg_gain * (n - 1) as f64 + g) / n as f64;
        avg_loss = (avg_loss * (n - 1) as f64 + l) / n as f64;
    }
    if avg_loss == 0.0 {
        return Some(100.0);
    }
    let rs = avg_gain / avg_loss;
    Some(100.0 - 100.0 / (1.0 + rs))
}

/// 经典 KDJ：RSV(9) → K = (2/3) prev_K + (1/3) RSV，D = (2/3) prev_D + (1/3) K，J = 3K − 2D。
/// 初始 K/D = 50。窗口不足返 None。
fn kdj_series(
    closes: &[f64],
    highs: &[f64],
    lows: &[f64],
    n: usize,
    _m1: usize,
    _m2: usize,
) -> (Option<f64>, Option<f64>, Option<f64>) {
    let len = closes.len();
    if len < n || n == 0 {
        return (None, None, None);
    }
    let mut k = 50.0f64;
    let mut d = 50.0f64;
    for i in (n - 1)..len {
        let hi = highs[i + 1 - n..=i].iter().cloned().fold(f64::MIN, f64::max);
        let lo = lows[i + 1 - n..=i].iter().cloned().fold(f64::MAX, f64::min);
        let rsv = if hi == lo {
            50.0
        } else {
            (closes[i] - lo) / (hi - lo) * 100.0
        };
        k = 2.0 / 3.0 * k + 1.0 / 3.0 * rsv;
        d = 2.0 / 3.0 * d + 1.0 / 3.0 * k;
    }
    let j = 3.0 * k - 2.0 * d;
    (Some(k), Some(d), Some(j))
}

/// MA20 ± 2σ（population stddev，spec）。窗口不足返 None。
fn boll(closes: &[f64], n: usize, upper: bool) -> Option<f64> {
    if closes.len() < n {
        return None;
    }
    let window = &closes[closes.len() - n..];
    let mean = window.iter().sum::<f64>() / n as f64;
    let var = window.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64;
    let std = var.sqrt();
    Some(if upper { mean + 2.0 * std } else { mean - 2.0 * std })
}

/// 历史 IndicatorBasis 构造助手。
pub fn make_basis(period: &str, adjust: &str, fetched_at: OccurredAt) -> IndicatorBasis {
    IndicatorBasis {
        period: period.into(),
        adjust: adjust.into(),
        fetched_at,
    }
}

#[allow(dead_code)] // 历史 trade_date helpers（接入 K 线后启用）
pub fn last_kline_date(klines: &[KlinePoint]) -> Option<TradeDate> {
    klines.last().map(|k| k.date)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shared::{Lots, Yuan};

    fn k(close: f64, high: f64, low: f64) -> KlinePoint {
        KlinePoint {
            date: TradeDate::from_unchecked(20260101),
            open: Yuan::from_unchecked(close),
            close: Yuan::from_unchecked(close),
            high: Yuan::from_unchecked(high),
            low: Yuan::from_unchecked(low),
            volume: Lots::from_unchecked(1000),
            amount: Yuan::from_unchecked(close * 1000.0),
        }
    }

    #[test]
    fn ma5_basic() {
        let ks: Vec<KlinePoint> = (1..=10).map(|i| k(i as f64, i as f64, i as f64)).collect();
        let basis = IndicatorBasis {
            period: "day".into(),
            adjust: "none".into(),
            fetched_at: OccurredAt::new(0),
        };
        let snap = compute_indicators("000001.SZ", &ks, basis, Some(&[IndicatorName::Ma5]));
        assert_eq!(snap.values[&IndicatorName::Ma5], Some(8.0)); // mean(6..=10) = 8
    }

    #[test]
    fn ma_window_too_small_is_none() {
        let ks: Vec<KlinePoint> = (1..=3).map(|i| k(i as f64, i as f64, i as f64)).collect();
        let basis = IndicatorBasis {
            period: "day".into(),
            adjust: "none".into(),
            fetched_at: OccurredAt::new(0),
        };
        let snap = compute_indicators("000001.SZ", &ks, basis, Some(&[IndicatorName::Ma5]));
        assert_eq!(snap.values[&IndicatorName::Ma5], None);
    }

    #[test]
    fn all_20_indicator_names_unique() {
        let all = IndicatorName::all();
        let mut s: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for n in all {
            assert!(s.insert(n.as_str()), "duplicate: {}", n.as_str());
        }
        assert_eq!(s.len(), 20);
    }
}
