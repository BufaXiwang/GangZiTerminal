//! Signal detector——24 个 SignalKind 的纯代码检测器（视觉信号除外）。
//!
//! 见 docs/design/agent-v3-expectation-driven.md § 3 + § 5.2 阶段 1。
//!
//! 设计原则：
//! - **纯函数**：输入 K 线 + indicators + quote + 资金面数据，输出 `Vec<SignalKind>`
//! - **0 LLM**：所有判定都是阈值 / 几何 / 跨日对比
//! - **批量**：一次扫一只股的所有 signals，方便 scan_tick 高频调用
//! - **视觉信号 + 消息信号**：本模块**不**生成——由 `analyze_chart` 工具链 + news tagger
//!   分别负责，scan_tick 主入口在汇总阶段合并所有来源
//!
//! Phase 1 实现完整度：
//! - ✅ 趋势 / 动量（8）：基于 indicators 完整实现
//! - ✅ 摆动 / 均值回归（4）：基于 indicators 完整实现
//! - ✅ 量能（2/3）：VolumeSpike / VolumeShrink 完整；VolumePriceDivergence 标记 TODO
//! - ✅ A 股特殊（3）：基于 quote 完整实现
//! - 🚧 资金 / 板块 / 因子 / Upcoming Event：留 W23 scan 集成时按需调用专用 worker

use crate::domain::shared::signal::SignalKind;
use crate::domain::quotes::indicators::IndicatorSnapshot;
use crate::domain::quotes::types::{KlinePoint, StockQuote};

/// 阈值默认值集合——后续可由 strategy 覆盖。
#[derive(Debug, Clone, Copy)]
pub struct DetectorConfig {
    pub rsi_oversold_threshold: f64,
    pub rsi_overbought_threshold: f64,
    pub volume_spike_ratio: f32,
    pub volume_shrink_ratio: f32,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            rsi_oversold_threshold: 30.0,
            rsi_overbought_threshold: 70.0,
            volume_spike_ratio: 1.5,
            volume_shrink_ratio: 0.5,
        }
    }
}

/// 单股扫描入口——返回触发的所有 SignalKind。
///
/// 调用方负责拉数据：`klines` 是近 60 日（升序），`snap` 是 `compute_indicators(klines, cfg)`
/// 的结果，`quote` 是当前实时报价。`prev_snap` 是上一交易日的 indicators 快照（用于
/// 检测金叉死叉等跨日事件，可为 None——首次跑时跳过这类信号）。
pub fn scan_one(
    klines: &[KlinePoint],
    snap: &IndicatorSnapshot,
    prev_snap: Option<&IndicatorSnapshot>,
    quote: Option<&StockQuote>,
    cfg: &DetectorConfig,
) -> Vec<SignalKind> {
    let mut out = Vec::new();
    detect_trend_momentum(klines, snap, prev_snap, &mut out);
    detect_oscillator(snap, cfg, &mut out);
    detect_volume(snap, cfg, &mut out);
    if let Some(q) = quote {
        detect_a_share_special(q, &mut out);
    }
    out
}

// ====== 趋势 / 动量（8 个 detector） ====================================

fn detect_trend_momentum(
    klines: &[KlinePoint],
    snap: &IndicatorSnapshot,
    prev: Option<&IndicatorSnapshot>,
    out: &mut Vec<SignalKind>,
) {
    let close = snap.close.value();
    // BreakoutAbove/Below20MA：当日 close 突破/跌破 20 日 MA
    if let Some(ma20) = snap.ma.get(&20) {
        let prev_close_above = prev
            .and_then(|p| p.ma.get(&20))
            .map(|m| prev.unwrap().close.value() >= *m)
            .unwrap_or(false);
        let now_above = close >= *ma20;
        if now_above && prev.is_some() && !prev_close_above {
            out.push(SignalKind::BreakoutAbove20MA);
        } else if !now_above && prev.is_some() && prev_close_above {
            out.push(SignalKind::BreakoutBelow20MA);
        }
    }
    // MA5 cross MA20
    if let (Some(ma5), Some(ma20)) = (snap.ma.get(&5), snap.ma.get(&20)) {
        let prev_ma5_above = prev
            .and_then(|p| Some(p.ma.get(&5)? > p.ma.get(&20)?))
            .unwrap_or(false);
        let now_above = ma5 > ma20;
        if now_above && prev.is_some() && !prev_ma5_above {
            out.push(SignalKind::MA5CrossAbove20);
        } else if !now_above && prev.is_some() && prev_ma5_above {
            out.push(SignalKind::MA5CrossBelow20);
        }
    }
    // MACD 金叉 / 死叉
    if let (Some((dif, dea, _)), Some((p_dif, p_dea, _))) =
        (snap.macd, prev.and_then(|p| p.macd))
    {
        let now_golden = dif > dea;
        let prev_golden = p_dif > p_dea;
        if now_golden && !prev_golden {
            out.push(SignalKind::MACDGoldenCross);
        } else if !now_golden && prev_golden {
            out.push(SignalKind::MACDDeathCross);
        }
    }
    // 20 日新高 / 新低
    let lookback = 20usize.min(klines.len());
    if lookback >= 2 {
        let window = &klines[klines.len() - lookback..klines.len() - 1]; // 不含最后一条
        let max_high = window.iter().map(|k| k.high.value()).fold(f64::MIN, f64::max);
        let min_low = window.iter().map(|k| k.low.value()).fold(f64::MAX, f64::min);
        let last = klines.last().unwrap();
        if last.high.value() > max_high {
            out.push(SignalKind::New20DayHigh);
        }
        if last.low.value() < min_low {
            out.push(SignalKind::New20DayLow);
        }
    }
}

// ====== 摆动 / 均值回归（4 个 detector） ================================

fn detect_oscillator(
    snap: &IndicatorSnapshot,
    cfg: &DetectorConfig,
    out: &mut Vec<SignalKind>,
) {
    if let Some(rsi) = snap.rsi.get(&14) {
        if *rsi < cfg.rsi_oversold_threshold {
            out.push(SignalKind::RSIOversold { period: 14 });
        } else if *rsi > cfg.rsi_overbought_threshold {
            out.push(SignalKind::RSIOverbought { period: 14 });
        }
    }
    if let Some((_mid, upper, lower)) = snap.boll {
        let close = snap.close.value();
        if close >= upper {
            out.push(SignalKind::BollingerBreakUpper);
        } else if close <= lower {
            out.push(SignalKind::BollingerBreakLower);
        }
    }
}

// ====== 量能（2/3 个 detector） =========================================

fn detect_volume(snap: &IndicatorSnapshot, cfg: &DetectorConfig, out: &mut Vec<SignalKind>) {
    if let Some(ratio) = snap.volume_ratio {
        if (ratio as f32) >= cfg.volume_spike_ratio {
            out.push(SignalKind::VolumeSpike {
                ratio: ratio as f32,
            });
        } else if (ratio as f32) <= cfg.volume_shrink_ratio {
            out.push(SignalKind::VolumeShrink {
                ratio: ratio as f32,
            });
        }
    }
    // VolumePriceDivergence：W23 接 scan_tick 时按需基于 OBV 序列实现
}

// ====== A 股特殊（3 个 detector） =======================================

fn detect_a_share_special(quote: &StockQuote, out: &mut Vec<SignalKind>) {
    // 涨跌停判定：基于 change_percent 接近 ±10% / 20% / 30%（按 market_prefix 区分）
    // Phase 1 简化：>9.9% 算涨停，<-9.9% 算跌停。后续按 code 前缀细化（创业板/科创/北交所）
    let Some(change_pct) = quote.change_percent else {
        return;
    };
    if change_pct >= 9.9 {
        out.push(SignalKind::LimitUp);
        // 一字板：开 = 高 = 低 = 收 = 涨停价（量能小）
        if let (Some(open), Some(high), Some(low), Some(close)) =
            (quote.open, quote.high, quote.low, quote.price)
        {
            let same = (open.value() - close.value()).abs() < 0.01
                && (high.value() - close.value()).abs() < 0.01
                && (low.value() - close.value()).abs() < 0.01;
            if same {
                out.push(SignalKind::LimitUpFlooded);
            }
        }
    } else if change_pct <= -9.9 {
        out.push(SignalKind::LimitDown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::quotes::indicators::{compute_indicators, IndicatorConfig};
    use crate::domain::quotes::types::KlinePoint;
    use crate::domain::shared::{Lots, TradeDate, Yuan};

    fn synthetic_kline(close: f64, volume: i64, date: i32) -> KlinePoint {
        KlinePoint {
            date: TradeDate::from_unchecked(date),
            open: Yuan::new(close).unwrap(),
            close: Yuan::new(close).unwrap(),
            high: Yuan::new(close * 1.01).unwrap(),
            low: Yuan::new(close * 0.99).unwrap(),
            volume: Lots::from_unchecked(volume),
            amount: Yuan::from_unchecked(volume as f64 * close),
        }
    }

    #[test]
    fn detect_new_20day_high() {
        let mut klines: Vec<KlinePoint> = (0..30)
            .map(|i| synthetic_kline(100.0, 10_000, 20260100 + i))
            .collect();
        let last = klines.last_mut().unwrap();
        last.high = Yuan::new(120.0).unwrap();
        last.close = Yuan::new(115.0).unwrap();
        let snap = compute_indicators(&klines, &IndicatorConfig::default()).unwrap();
        let mut out = Vec::new();
        detect_trend_momentum(&klines, &snap, None, &mut out);
        assert!(out.contains(&SignalKind::New20DayHigh));
    }

    #[test]
    fn detect_volume_spike() {
        let mut klines: Vec<KlinePoint> = (0..10)
            .map(|i| synthetic_kline(100.0, 10_000, 20260100 + i))
            .collect();
        klines.last_mut().unwrap().volume = Lots::from_unchecked(50_000);
        let snap = compute_indicators(&klines, &IndicatorConfig::default()).unwrap();
        let mut out = Vec::new();
        detect_volume(&snap, &DetectorConfig::default(), &mut out);
        assert!(matches!(
            out.first(),
            Some(SignalKind::VolumeSpike { .. })
        ));
    }
}
