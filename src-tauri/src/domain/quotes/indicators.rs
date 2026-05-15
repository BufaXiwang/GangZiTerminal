//! 技术指标计算——纯函数 + 可配参数。
//!
//! 全部 pure：接 `&[KlinePoint]` + `IndicatorConfig`，返 `IndicatorSnapshot`。
//! 无 I/O，无外部依赖（除 std + domain shared）。
//!
//! 公式参照标准实现：
//! - MA：简单算术平均
//! - EMA：α=2/(n+1) 指数加权
//! - MACD：EMA(12) - EMA(26)，signal = EMA(DIF, 9)，hist = (DIF - DEA) × 2
//! - RSI：Wilder 平滑差分
//! - KDJ：经典 9/3/3，K/D 用 α=1/m 平滑，J = 3K - 2D
//! - BOLL：MA(20) ± k × σ
//! - ATR：Wilder 平滑 TR
//! - OBV：累计 sign(Δclose) × volume

use super::types::KlinePoint;
use crate::domain::shared::Yuan;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ============================================================================
// 配置
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndicatorConfig {
    pub ma_periods: Vec<u32>,
    pub ema_periods: Vec<u32>,
    pub rsi_periods: Vec<u32>,
    /// (short, long, signal)
    pub macd: (u32, u32, u32),
    /// (n, m1, m2)
    pub kdj: (u32, u32, u32),
    /// (period, k_multiplier)
    pub boll: (u32, f64),
    pub atr_period: u32,
    pub cci_period: u32,
}

impl Default for IndicatorConfig {
    fn default() -> Self {
        Self {
            ma_periods: vec![5, 10, 20, 60, 120],
            ema_periods: vec![12, 26],
            rsi_periods: vec![6, 14, 24],
            macd: (12, 26, 9),
            kdj: (9, 3, 3),
            boll: (20, 2.0),
            atr_period: 14,
            cci_period: 14,
        }
    }
}

// ============================================================================
// 输出快照
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndicatorSnapshot {
    pub close: Yuan,
    pub as_of: crate::domain::shared::TradeDate,
    pub ma: BTreeMap<u32, f64>,
    pub ema: BTreeMap<u32, f64>,
    /// (DIF, DEA, HIST)
    pub macd: Option<(f64, f64, f64)>,
    pub rsi: BTreeMap<u32, f64>,
    /// (K, D, J)
    pub kdj: Option<(f64, f64, f64)>,
    pub cci: Option<f64>,
    /// (mid, upper, lower)
    pub boll: Option<(f64, f64, f64)>,
    pub atr: Option<f64>,
    pub obv: Option<f64>,
    pub volume_ratio: Option<f64>,
}

// ============================================================================
// 主入口（纯函数）
// ============================================================================

pub fn compute_indicators(
    klines: &[KlinePoint],
    cfg: &IndicatorConfig,
) -> Option<IndicatorSnapshot> {
    let last = klines.last()?;
    let closes: Vec<f64> = klines.iter().map(|k| k.close.value()).collect();

    let ma: BTreeMap<u32, f64> = cfg
        .ma_periods
        .iter()
        .filter_map(|p| ma_last(&closes, *p as usize).map(|v| (*p, v)))
        .collect();

    let ema: BTreeMap<u32, f64> = cfg
        .ema_periods
        .iter()
        .filter_map(|p| ema_last(&closes, *p as usize).map(|v| (*p, v)))
        .collect();

    let macd = macd(
        &closes,
        cfg.macd.0 as usize,
        cfg.macd.1 as usize,
        cfg.macd.2 as usize,
    );

    let rsi: BTreeMap<u32, f64> = cfg
        .rsi_periods
        .iter()
        .filter_map(|p| rsi_wilder(&closes, *p as usize).map(|v| (*p, v)))
        .collect();

    let kdj = kdj(
        klines,
        cfg.kdj.0 as usize,
        cfg.kdj.1 as usize,
        cfg.kdj.2 as usize,
    );
    let cci = cci(klines, cfg.cci_period as usize);
    let boll = boll(&closes, cfg.boll.0 as usize, cfg.boll.1);
    let atr = atr(klines, cfg.atr_period as usize);
    let obv = obv(klines);
    let volume_ratio = volume_ratio(klines, 5);

    Some(IndicatorSnapshot {
        close: last.close,
        as_of: last.date,
        ma,
        ema,
        macd,
        rsi,
        kdj,
        cci,
        boll,
        atr,
        obv,
        volume_ratio,
    })
}

// ============================================================================
// 基础公式
// ============================================================================

fn ma_last(values: &[f64], window: usize) -> Option<f64> {
    if window == 0 || values.len() < window {
        return None;
    }
    let slice = &values[values.len() - window..];
    Some(slice.iter().sum::<f64>() / window as f64)
}

fn ema_series(values: &[f64], window: usize) -> Vec<f64> {
    if values.is_empty() || window == 0 {
        return Vec::new();
    }
    let alpha = 2.0 / (window as f64 + 1.0);
    let seed_end = window.min(values.len());
    let seed = values[..seed_end].iter().sum::<f64>() / seed_end as f64;
    let mut out = Vec::with_capacity(values.len());
    out.push(seed);
    for i in seed_end..values.len() {
        let prev = *out.last().unwrap();
        out.push(alpha * values[i] + (1.0 - alpha) * prev);
    }
    out
}

fn ema_last(values: &[f64], window: usize) -> Option<f64> {
    ema_series(values, window).last().copied()
}

fn macd(closes: &[f64], short: usize, long: usize, signal: usize) -> Option<(f64, f64, f64)> {
    if closes.len() < long {
        return None;
    }
    let ema_s = ema_series(closes, short);
    let ema_l = ema_series(closes, long);
    let n = ema_s.len().min(ema_l.len());
    let dif_series: Vec<f64> = (0..n)
        .map(|i| {
            let ofs_s = ema_s.len() - n;
            let ofs_l = ema_l.len() - n;
            ema_s[ofs_s + i] - ema_l[ofs_l + i]
        })
        .collect();
    let dea_series = ema_series(&dif_series, signal);
    let dif = *dif_series.last()?;
    let dea = *dea_series.last()?;
    let hist = (dif - dea) * 2.0;
    Some((dif, dea, hist))
}

fn rsi_wilder(closes: &[f64], period: usize) -> Option<f64> {
    if period == 0 || closes.len() < period + 1 {
        return None;
    }
    let mut gain = 0.0;
    let mut loss = 0.0;
    for i in 1..=period {
        let d = closes[i] - closes[i - 1];
        if d >= 0.0 {
            gain += d;
        } else {
            loss -= d;
        }
    }
    gain /= period as f64;
    loss /= period as f64;
    let p = period as f64;
    for i in (period + 1)..closes.len() {
        let d = closes[i] - closes[i - 1];
        let (g, l) = if d >= 0.0 { (d, 0.0) } else { (0.0, -d) };
        gain = (gain * (p - 1.0) + g) / p;
        loss = (loss * (p - 1.0) + l) / p;
    }
    if loss == 0.0 {
        return Some(100.0);
    }
    let rs = gain / loss;
    Some(100.0 - 100.0 / (1.0 + rs))
}

fn kdj(klines: &[KlinePoint], n: usize, m1: usize, m2: usize) -> Option<(f64, f64, f64)> {
    if klines.len() < n {
        return None;
    }
    let mut k_prev = 50.0;
    let mut d_prev = 50.0;
    let a1 = 1.0 / m1 as f64;
    let a2 = 1.0 / m2 as f64;
    for i in (n - 1)..klines.len() {
        let win = &klines[i + 1 - n..=i];
        let hh = win
            .iter()
            .map(|k| k.high.value())
            .fold(f64::NEG_INFINITY, f64::max);
        let ll = win
            .iter()
            .map(|k| k.low.value())
            .fold(f64::INFINITY, f64::min);
        let close = klines[i].close.value();
        let rsv = if hh == ll {
            50.0
        } else {
            (close - ll) / (hh - ll) * 100.0
        };
        k_prev = a1 * rsv + (1.0 - a1) * k_prev;
        d_prev = a2 * k_prev + (1.0 - a2) * d_prev;
    }
    let j = 3.0 * k_prev - 2.0 * d_prev;
    Some((k_prev, d_prev, j))
}

fn cci(klines: &[KlinePoint], period: usize) -> Option<f64> {
    if period == 0 || klines.len() < period {
        return None;
    }
    let tps: Vec<f64> = klines
        .iter()
        .map(|k| (k.high.value() + k.low.value() + k.close.value()) / 3.0)
        .collect();
    let recent = &tps[tps.len() - period..];
    let ma_tp = recent.iter().sum::<f64>() / period as f64;
    let mad = recent.iter().map(|x| (x - ma_tp).abs()).sum::<f64>() / period as f64;
    if mad == 0.0 {
        return Some(0.0);
    }
    Some((tps.last().unwrap() - ma_tp) / (0.015 * mad))
}

fn boll(closes: &[f64], period: usize, k: f64) -> Option<(f64, f64, f64)> {
    if period == 0 || closes.len() < period {
        return None;
    }
    let slice = &closes[closes.len() - period..];
    let mid = slice.iter().sum::<f64>() / period as f64;
    let var = slice.iter().map(|x| (x - mid).powi(2)).sum::<f64>() / period as f64;
    let std = var.sqrt();
    Some((mid, mid + k * std, mid - k * std))
}

fn atr(klines: &[KlinePoint], period: usize) -> Option<f64> {
    if period == 0 || klines.len() < period + 1 {
        return None;
    }
    let mut trs = Vec::with_capacity(klines.len() - 1);
    for i in 1..klines.len() {
        let pc = klines[i - 1].close.value();
        let h = klines[i].high.value();
        let l = klines[i].low.value();
        let tr = (h - l).max((h - pc).abs()).max((l - pc).abs());
        trs.push(tr);
    }
    if trs.len() < period {
        return None;
    }
    let mut atr_val = trs[..period].iter().sum::<f64>() / period as f64;
    let p = period as f64;
    for tr in &trs[period..] {
        atr_val = (atr_val * (p - 1.0) + tr) / p;
    }
    Some(atr_val)
}

fn obv(klines: &[KlinePoint]) -> Option<f64> {
    if klines.len() < 2 {
        return None;
    }
    let mut v = 0.0;
    for i in 1..klines.len() {
        let vol = klines[i].volume.value() as f64;
        match klines[i]
            .close
            .value()
            .partial_cmp(&klines[i - 1].close.value())
        {
            Some(std::cmp::Ordering::Greater) => v += vol,
            Some(std::cmp::Ordering::Less) => v -= vol,
            _ => {}
        }
    }
    Some(v)
}

fn volume_ratio(klines: &[KlinePoint], baseline: usize) -> Option<f64> {
    if klines.len() < baseline + 1 {
        return None;
    }
    let last_vol = klines.last()?.volume.value() as f64;
    let baseline_avg = klines[klines.len() - baseline - 1..klines.len() - 1]
        .iter()
        .map(|k| k.volume.value() as f64)
        .sum::<f64>()
        / baseline as f64;
    if baseline_avg <= 0.0 {
        return None;
    }
    Some(last_vol / baseline_avg)
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shared::{Lots, TradeDate};

    fn k(
        y: i32,
        m: i32,
        d: i32,
        open: f64,
        high: f64,
        low: f64,
        close: f64,
        vol: i64,
    ) -> KlinePoint {
        KlinePoint {
            date: TradeDate::new(y * 10000 + m * 100 + d).unwrap(),
            open: Yuan::from_unchecked(open),
            close: Yuan::from_unchecked(close),
            high: Yuan::from_unchecked(high),
            low: Yuan::from_unchecked(low),
            volume: Lots::from_unchecked(vol),
            amount: Yuan::from_unchecked(close * vol as f64),
        }
    }

    #[test]
    fn ma_simple() {
        let v = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(ma_last(&v, 5), Some(3.0));
        assert_eq!(ma_last(&v, 6), None);
    }

    #[test]
    fn rsi_all_gains_returns_100() {
        let closes: Vec<f64> = (1..=20).map(|i| i as f64).collect();
        assert!((rsi_wilder(&closes, 14).unwrap() - 100.0).abs() < 1e-6);
    }

    #[test]
    fn boll_constant_zero_band() {
        let closes = vec![10.0; 25];
        let (m, u, l) = boll(&closes, 20, 2.0).unwrap();
        assert!((m - 10.0).abs() < 1e-9);
        assert!((u - 10.0).abs() < 1e-9);
        assert!((l - 10.0).abs() < 1e-9);
    }

    #[test]
    fn obv_accumulates() {
        let klines = vec![
            k(2026, 5, 1, 10.0, 11.0, 9.0, 10.5, 100),
            k(2026, 5, 2, 10.5, 11.5, 10.0, 11.0, 200), // up → +200
            k(2026, 5, 3, 11.0, 11.5, 10.5, 10.8, 150), // down → -150
        ];
        assert!((obv(&klines).unwrap() - 50.0).abs() < 1e-9);
    }

    #[test]
    fn compute_indicators_full_snapshot() {
        let klines: Vec<KlinePoint> = (1..=30)
            .map(|i| k(2026, 5, i, 10.0, 11.0, 9.0, 10.0 + i as f64 * 0.1, 100))
            .collect();
        let snap = compute_indicators(&klines, &IndicatorConfig::default()).unwrap();
        assert!(snap.ma.contains_key(&5));
        assert!(snap.macd.is_some());
        assert!(snap.rsi.contains_key(&14));
    }
}
