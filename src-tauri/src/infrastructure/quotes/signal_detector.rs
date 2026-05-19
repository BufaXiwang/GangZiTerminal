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
use crate::domain::quotes::types::{
    DailyBasic, KlinePoint, NorthMoneyFlow, StockQuote, TopListItem,
};

/// 阈值默认值集合——后续可由 strategy 覆盖。
#[derive(Debug, Clone, Copy)]
pub struct DetectorConfig {
    pub rsi_oversold_threshold: f64,
    pub rsi_overbought_threshold: f64,
    pub volume_spike_ratio: f32,
    pub volume_shrink_ratio: f32,
    /// 北向连续净流入 / 流出阈值（默认 3 天）
    pub north_streak_days: u32,
    /// PB 触发阈值
    pub pb_threshold: f32,
    /// ROE 触发阈值
    pub roe_threshold_pct: f32,
    /// 净利润增长率触发阈值
    pub earnings_growth_threshold_pct: f32,
    /// 板块涨幅触发阈值
    pub sector_strength_pct: f32,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            rsi_oversold_threshold: 30.0,
            rsi_overbought_threshold: 70.0,
            volume_spike_ratio: 1.5,
            volume_shrink_ratio: 0.5,
            north_streak_days: 3,
            pb_threshold: 1.0,
            roe_threshold_pct: 15.0,
            earnings_growth_threshold_pct: 30.0,
            sector_strength_pct: 3.0,
        }
    }
}

/// 扩展数据上下文——调用方按需预先 fetch；检测函数纯。
///
/// 凡是字段 None 的，对应 detector 自动跳过——这让 scan_tick 可以在不同 budget
/// 下按 tier 注入不同数据量（quick tick 只带 quote/klines；full tick 带全套）。
#[derive(Debug, Clone, Default)]
pub struct ScanContext<'a> {
    /// 最新 daily_basic（含 PE / PB / 市值）
    pub fundamentals: Option<&'a DailyBasic>,
    /// 北向资金近 N 日序列（升序）
    pub north_flow: Option<&'a [NorthMoneyFlow]>,
    /// 今日龙虎榜——若该 code 在列表里则触发 OnDragonTigerList
    pub dragon_tiger_today: Option<&'a [TopListItem]>,
    /// 所在板块今日涨幅（%）
    pub sector_pct_change_today: Option<f64>,
    /// 净利润 YoY 增速（%）
    pub earnings_growth_yoy_pct: Option<f64>,
    /// 最新 ROE（%）
    pub roe_pct: Option<f64>,
    /// 即将到来的事件——(EventKind, days_ahead)，days_ahead<=7 视为 upcoming
    pub upcoming_events: Option<&'a [(crate::domain::shared::signal::EventKind, u32)]>,
}

/// 单股扫描入口——返回触发的所有 SignalKind。
/// 带上下文的扫描——上下文中存在的字段会被对应 detector 消费。
///
/// `klines` 近 60 日（升序），`snap` 是 `compute_indicators(klines, cfg)` 的结果，
/// `quote` 是当前实时报价。`prev_snap` 是上一交易日的 indicators 快照（金叉/死叉
/// 等跨日事件用，可为 None——首次跑时跳过这类信号）。
pub fn scan_one_with_context(
    klines: &[KlinePoint],
    snap: &IndicatorSnapshot,
    prev_snap: Option<&IndicatorSnapshot>,
    quote: Option<&StockQuote>,
    cfg: &DetectorConfig,
    ctx: &ScanContext<'_>,
) -> Vec<SignalKind> {
    let mut out = Vec::new();
    detect_trend_momentum(klines, snap, prev_snap, &mut out);
    detect_oscillator(snap, cfg, &mut out);
    detect_volume(snap, prev_snap, cfg, &mut out);
    if let Some(q) = quote {
        detect_a_share_special(q, &mut out);
    }
    detect_capital_flow(quote, ctx, cfg, &mut out);
    detect_fundamentals(ctx, cfg, &mut out);
    detect_sector_and_events(ctx, cfg, &mut out);
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

// ====== 量能（3 个 detector） ===========================================

fn detect_volume(
    snap: &IndicatorSnapshot,
    prev: Option<&IndicatorSnapshot>,
    cfg: &DetectorConfig,
    out: &mut Vec<SignalKind>,
) {
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
    // 量价背离：今日 close > 昨日，但 OBV 下降（或反之）。需要 prev 才能比较趋势。
    if let (Some(prev), Some(obv_now), Some(obv_prev)) = (
        prev,
        snap.obv,
        prev.and_then(|p| p.obv),
    ) {
        let price_up = snap.close.value() > prev.close.value();
        let obv_up = obv_now > obv_prev;
        if price_up != obv_up {
            out.push(SignalKind::VolumePriceDivergence);
        }
    }
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

// ====== 资金 / 主力（3 个 detector） =====================================

fn detect_capital_flow(
    quote: Option<&StockQuote>,
    ctx: &ScanContext<'_>,
    cfg: &DetectorConfig,
    out: &mut Vec<SignalKind>,
) {
    if let Some(series) = ctx.north_flow {
        let need = cfg.north_streak_days as usize;
        if series.len() >= need {
            let tail = &series[series.len() - need..];
            let all_in = tail.iter().all(|n| n.total.value() > 0.0);
            let all_out = tail.iter().all(|n| n.total.value() < 0.0);
            if all_in {
                out.push(SignalKind::NorthInflowStreak {
                    days: cfg.north_streak_days,
                });
            } else if all_out {
                out.push(SignalKind::NorthOutflowStreak {
                    days: cfg.north_streak_days,
                });
            }
        }
    }
    if let (Some(q), Some(list)) = (quote, ctx.dragon_tiger_today) {
        if list.iter().any(|item| item.code == q.code) {
            out.push(SignalKind::OnDragonTigerList);
        }
    }
}

// ====== 基本面因子（4 个 detector） ======================================

fn detect_fundamentals(ctx: &ScanContext<'_>, cfg: &DetectorConfig, out: &mut Vec<SignalKind>) {
    if let Some(fund) = ctx.fundamentals {
        if let Some(pb) = fund.pb {
            if (pb as f32) <= cfg.pb_threshold {
                out.push(SignalKind::PBBelowThreshold {
                    value: cfg.pb_threshold,
                });
            }
        }
        // PEBelowSectorPct：需要"PE 在板块内的百分位"——板块 PE 序列没装。
        // 现阶段降级：若 PE 在 [0, 15] 视为偏低 1 分位（占位 pct=20）。
        if let Some(pe) = fund.pe_ttm.or(fund.pe) {
            if pe > 0.0 && pe <= 15.0 {
                out.push(SignalKind::PEBelowSectorPct { pct: 20.0 });
            }
        }
    }
    if let Some(roe) = ctx.roe_pct {
        if (roe as f32) >= cfg.roe_threshold_pct {
            out.push(SignalKind::ROEAboveThreshold {
                pct: cfg.roe_threshold_pct,
            });
        }
    }
    if let Some(g) = ctx.earnings_growth_yoy_pct {
        if (g as f32) >= cfg.earnings_growth_threshold_pct {
            out.push(SignalKind::EarningsGrowthAbove {
                pct: cfg.earnings_growth_threshold_pct,
            });
        }
    }
}

// ====== 板块 / 事件（2 个 detector） =====================================

fn detect_sector_and_events(
    ctx: &ScanContext<'_>,
    cfg: &DetectorConfig,
    out: &mut Vec<SignalKind>,
) {
    if let Some(pct) = ctx.sector_pct_change_today {
        if (pct as f32) >= cfg.sector_strength_pct {
            out.push(SignalKind::SectorStrengthAbove {
                pct: cfg.sector_strength_pct,
            });
        }
    }
    if let Some(events) = ctx.upcoming_events {
        for (kind, days_ahead) in events {
            if *days_ahead <= 7 {
                out.push(SignalKind::UpcomingEvent {
                    event_kind: *kind,
                    days_ahead: *days_ahead,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::quotes::indicators::{compute_indicators, IndicatorConfig};
    use crate::domain::quotes::types::{DailyBasic, KlinePoint, NorthMoneyFlow, TopListItem};
    use crate::domain::shared::{Lots, StockCode, TradeDate, Yuan};

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
        detect_volume(&snap, None, &DetectorConfig::default(), &mut out);
        assert!(matches!(
            out.first(),
            Some(SignalKind::VolumeSpike { .. })
        ));
    }

    #[test]
    fn detect_north_inflow_streak() {
        let series: Vec<NorthMoneyFlow> = (0..3)
            .map(|i| NorthMoneyFlow {
                trade_date: TradeDate::from_unchecked(20260101 + i),
                sh_north: Yuan::from_unchecked(1.0),
                sz_north: Yuan::from_unchecked(1.0),
                total: Yuan::from_unchecked(2.0),
            })
            .collect();
        let ctx = ScanContext {
            north_flow: Some(&series),
            ..Default::default()
        };
        let mut out = Vec::new();
        detect_capital_flow(None, &ctx, &DetectorConfig::default(), &mut out);
        assert!(matches!(
            out.first(),
            Some(SignalKind::NorthInflowStreak { .. })
        ));
    }

    #[test]
    fn detect_pb_below_threshold() {
        let fund = DailyBasic {
            code: StockCode::new("600000").unwrap(),
            trade_date: TradeDate::from_unchecked(20260101),
            pe: Some(20.0),
            pe_ttm: Some(20.0),
            pb: Some(0.8),
            ps: None,
            ps_ttm: None,
            dv_ratio: None,
            dv_ttm: None,
            turnover_rate: 1.0,
            turnover_rate_float: None,
            volume_ratio: 1.0,
            total_mv: Yuan::from_unchecked(0.0),
            circ_mv: Yuan::from_unchecked(0.0),
        };
        let ctx = ScanContext {
            fundamentals: Some(&fund),
            ..Default::default()
        };
        let mut out = Vec::new();
        detect_fundamentals(&ctx, &DetectorConfig::default(), &mut out);
        assert!(out.iter().any(|s| matches!(s, SignalKind::PBBelowThreshold { .. })));
    }

    #[test]
    fn detect_dragon_tiger() {
        use crate::domain::quotes::types::StockQuote;
        use crate::domain::shared::OccurredAt;
        let code = StockCode::new("600519").unwrap();
        let quote = StockQuote {
            code: code.clone(),
            name: "test".into(),
            price: Some(Yuan::from_unchecked(100.0)),
            change_percent: None,
            change: None,
            open: None,
            high: None,
            low: None,
            previous_close: None,
            day_volume: None,
            day_amount: None,
            captured_at: OccurredAt::new(0),
            bid_levels: vec![],
            ask_levels: vec![],
            buy_volume: None,
            sell_volume: None,
            order_imbalance: None,
        };
        let item = TopListItem {
            trade_date: TradeDate::from_unchecked(20260101),
            code: code.clone(),
            name: "test".into(),
            close: None,
            pct_change: None,
            turnover_rate: None,
            amount: None,
            net_amount: None,
            net_rate: None,
            reason: String::new(),
        };
        let list = vec![item];
        let ctx = ScanContext {
            dragon_tiger_today: Some(&list),
            ..Default::default()
        };
        let mut out = Vec::new();
        detect_capital_flow(Some(&quote), &ctx, &DetectorConfig::default(), &mut out);
        assert!(out.contains(&SignalKind::OnDragonTigerList));
    }
}
