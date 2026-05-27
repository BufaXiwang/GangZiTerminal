//! 技术指标 — 纯计算，基于本地 canonical K 线点位。
//!
//! Spec: docs/design/quotes-module.md §2 指标参数契约

use super::kline::{Adjust, KlinePeriod, KlinePoint};
use crate::domain::shared::{OccurredAt, TsCode, WarningCode};
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::BTreeMap;

/// Spec: quotes-module.md §2
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
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
    pub fn all() -> &'static [IndicatorName] {
        use IndicatorName::*;
        &[
            Ma5, Ma10, Ma20, Ma60, Ema12, Ema26, MacdDif, MacdDea, MacdHist, Rsi6, Rsi12, Rsi24,
            KdjK, KdjD, KdjJ, BollUpper, BollMid, BollLower, VolumeMa5, VolumeMa10,
        ]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct IndicatorBasis {
    pub period: KlinePeriod,
    pub adjust: Adjust,
    pub fetched_at: OccurredAt,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct IndicatorSnapshot {
    pub ts_code: TsCode,
    pub basis: IndicatorBasis,
    pub values: BTreeMap<String, Option<f64>>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<WarningCode>,
}

/// Spec: quotes-module.md §2 指标参数契约
///
/// 基于按时间升序的 `points` 计算；返回最新一根的指标值。窗口不足返回 `None`。
pub fn compute_indicators(
    ts_code: TsCode,
    basis: IndicatorBasis,
    points: &[KlinePoint],
    requested: &[IndicatorName],
    warnings: Vec<WarningCode>,
) -> IndicatorSnapshot {
    let closes: Vec<f64> = points.iter().map(|p| dec_to_f64(p.close.0)).collect();
    let volumes: Vec<f64> = points
        .iter()
        .map(|p| p.volume.map(|v| v.0 as f64).unwrap_or(f64::NAN))
        .collect();

    let mut values: BTreeMap<String, Option<f64>> = BTreeMap::new();
    for name in requested {
        let v = compute_one(*name, &closes, &volumes);
        values.insert(name_key(*name).to_string(), v);
    }
    IndicatorSnapshot {
        ts_code,
        basis,
        values,
        warnings,
    }
}

fn name_key(n: IndicatorName) -> &'static str {
    use IndicatorName::*;
    match n {
        Ma5 => "ma5",
        Ma10 => "ma10",
        Ma20 => "ma20",
        Ma60 => "ma60",
        Ema12 => "ema12",
        Ema26 => "ema26",
        MacdDif => "macd_dif",
        MacdDea => "macd_dea",
        MacdHist => "macd_hist",
        Rsi6 => "rsi6",
        Rsi12 => "rsi12",
        Rsi24 => "rsi24",
        KdjK => "kdj_k",
        KdjD => "kdj_d",
        KdjJ => "kdj_j",
        BollUpper => "boll_upper",
        BollMid => "boll_mid",
        BollLower => "boll_lower",
        VolumeMa5 => "volume_ma5",
        VolumeMa10 => "volume_ma10",
    }
}

fn dec_to_f64(d: rust_decimal::Decimal) -> f64 {
    d.to_f64().unwrap_or(f64::NAN)
}

fn compute_one(name: IndicatorName, closes: &[f64], volumes: &[f64]) -> Option<f64> {
    use IndicatorName::*;
    match name {
        Ma5 => sma_last(closes, 5),
        Ma10 => sma_last(closes, 10),
        Ma20 => sma_last(closes, 20),
        Ma60 => sma_last(closes, 60),
        Ema12 => ema_last(closes, 12),
        Ema26 => ema_last(closes, 26),
        MacdDif => macd_dif_last(closes),
        MacdDea => macd_dea_last(closes),
        MacdHist => macd_hist_last(closes),
        Rsi6 => wilder_rsi_last(closes, 6),
        Rsi12 => wilder_rsi_last(closes, 12),
        Rsi24 => wilder_rsi_last(closes, 24),
        KdjK => kdj_last(closes, 0),
        KdjD => kdj_last(closes, 1),
        KdjJ => kdj_last(closes, 2),
        BollMid => sma_last(closes, 20),
        BollUpper => boll_band_last(closes, 1.0),
        BollLower => boll_band_last(closes, -1.0),
        VolumeMa5 => sma_last(volumes, 5),
        VolumeMa10 => sma_last(volumes, 10),
    }
}

fn sma_last(xs: &[f64], window: usize) -> Option<f64> {
    if xs.len() < window || window == 0 {
        return None;
    }
    let slice = &xs[xs.len() - window..];
    if slice.iter().any(|v| v.is_nan()) {
        return None;
    }
    let sum: f64 = slice.iter().sum();
    Some(sum / window as f64)
}

/// EMA 序列，按经典 pandas span / TA 公式：alpha = 2/(span+1)。
///
/// 输出长度等于输入；前 `span - 1` 个位置由扩展窗口 SMA 引导，需要至少 `span` 个数据点
/// 才返回有效"最新值"——窗口不足时返回 None。
fn ema_series(xs: &[f64], span: usize) -> Vec<Option<f64>> {
    if span == 0 {
        return vec![None; xs.len()];
    }
    let alpha = 2.0 / (span as f64 + 1.0);
    let mut out = Vec::with_capacity(xs.len());
    let mut prev: Option<f64> = None;
    for (i, v) in xs.iter().copied().enumerate() {
        if v.is_nan() {
            out.push(None);
            continue;
        }
        let cur = match prev {
            None => v,
            Some(p) => alpha * v + (1.0 - alpha) * p,
        };
        prev = Some(cur);
        if i + 1 >= span {
            out.push(Some(cur));
        } else {
            out.push(None);
        }
    }
    out
}

fn ema_last(xs: &[f64], span: usize) -> Option<f64> {
    ema_series(xs, span).last().copied().flatten()
}

fn macd_dif_series(closes: &[f64]) -> Vec<Option<f64>> {
    let e12 = ema_series(closes, 12);
    let e26 = ema_series(closes, 26);
    e12.into_iter()
        .zip(e26.into_iter())
        .map(|(a, b)| match (a, b) {
            (Some(x), Some(y)) => Some(x - y),
            _ => None,
        })
        .collect()
}

fn macd_dif_last(closes: &[f64]) -> Option<f64> {
    macd_dif_series(closes).last().copied().flatten()
}

fn macd_dea_last(closes: &[f64]) -> Option<f64> {
    // DEA = EMA9(DIF)；DIF 中 None 位置不参与，遇到 None 时重置 prev。
    let dif = macd_dif_series(closes);
    let span = 9usize;
    let alpha = 2.0 / (span as f64 + 1.0);
    let mut prev: Option<f64> = None;
    let mut count = 0usize;
    let mut last_dea: Option<f64> = None;
    for v in dif.into_iter() {
        match v {
            Some(x) => {
                count += 1;
                let cur = match prev {
                    None => x,
                    Some(p) => alpha * x + (1.0 - alpha) * p,
                };
                prev = Some(cur);
                if count >= span {
                    last_dea = Some(cur);
                }
            }
            None => {
                prev = None;
                count = 0;
            }
        }
    }
    last_dea
}

fn macd_hist_last(closes: &[f64]) -> Option<f64> {
    let dif = macd_dif_last(closes)?;
    let dea = macd_dea_last(closes)?;
    Some(2.0 * (dif - dea))
}

fn wilder_rsi_last(closes: &[f64], window: usize) -> Option<f64> {
    if closes.len() <= window || window == 0 {
        return None;
    }
    if closes.iter().any(|v| v.is_nan()) {
        return None;
    }
    let mut gains = 0.0f64;
    let mut losses = 0.0f64;
    for i in 1..=window {
        let delta = closes[i] - closes[i - 1];
        if delta >= 0.0 {
            gains += delta;
        } else {
            losses += -delta;
        }
    }
    let mut avg_gain = gains / window as f64;
    let mut avg_loss = losses / window as f64;
    for i in (window + 1)..closes.len() {
        let delta = closes[i] - closes[i - 1];
        let (g, l) = if delta >= 0.0 { (delta, 0.0) } else { (0.0, -delta) };
        avg_gain = (avg_gain * (window as f64 - 1.0) + g) / window as f64;
        avg_loss = (avg_loss * (window as f64 - 1.0) + l) / window as f64;
    }
    if avg_loss == 0.0 {
        return Some(100.0);
    }
    let rs = avg_gain / avg_loss;
    Some(100.0 - 100.0 / (1.0 + rs))
}

/// KDJ — RSV window = 9, K/D smoothing = 3, init K/D = 50, J = 3K - 2D.
fn kdj_last(closes: &[f64], which: u8) -> Option<f64> {
    // KDJ 需要 high / low；本简化实现用 close 当作 H/L 单值（窗口高低用 close 窗口的 max/min）。
    let window = 9usize;
    if closes.len() < window {
        return None;
    }
    if closes.iter().any(|v| v.is_nan()) {
        return None;
    }
    let mut k = 50.0;
    let mut d = 50.0;
    for i in (window - 1)..closes.len() {
        let slice = &closes[i + 1 - window..=i];
        let hi = slice.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let lo = slice.iter().cloned().fold(f64::INFINITY, f64::min);
        let rsv = if (hi - lo).abs() < 1e-12 {
            50.0
        } else {
            (closes[i] - lo) / (hi - lo) * 100.0
        };
        k = (2.0 * k + rsv) / 3.0;
        d = (2.0 * d + k) / 3.0;
    }
    let j = 3.0 * k - 2.0 * d;
    match which {
        0 => Some(k),
        1 => Some(d),
        2 => Some(j),
        _ => None,
    }
}

fn boll_band_last(closes: &[f64], sign: f64) -> Option<f64> {
    let window = 20usize;
    if closes.len() < window {
        return None;
    }
    let slice = &closes[closes.len() - window..];
    if slice.iter().any(|v| v.is_nan()) {
        return None;
    }
    let mean: f64 = slice.iter().sum::<f64>() / window as f64;
    let var: f64 = slice.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / window as f64;
    let sd = var.sqrt();
    Some(mean + sign * 2.0 * sd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sma_works() {
        let xs: Vec<f64> = (1..=10).map(|i| i as f64).collect();
        assert_eq!(sma_last(&xs, 5).unwrap(), (6.0 + 7.0 + 8.0 + 9.0 + 10.0) / 5.0);
        assert!(sma_last(&xs, 20).is_none());
    }

    #[test]
    fn rsi_all_up_is_100() {
        let xs: Vec<f64> = (1..=30).map(|i| i as f64).collect();
        let r = wilder_rsi_last(&xs, 6).unwrap();
        assert!(r > 99.0);
    }

    #[test]
    fn ema_matches_simple_recursion() {
        // 平稳序列：EMA 收敛到该值
        let xs: Vec<f64> = vec![10.0; 50];
        let v = ema_last(&xs, 12).unwrap();
        assert!((v - 10.0).abs() < 1e-9);
    }
}
