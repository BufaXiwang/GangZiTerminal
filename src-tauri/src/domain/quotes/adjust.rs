//! 本地复权计算 — 基于 TDX xdxr 事件，对 unadjusted K 线现算 qfq / hfq。
//!
//! Spec: docs/design/quotes-module.md §2 "本地复权计算（基于 TDX xdxr）"
//!
//! 纯函数；无 I/O / 无 async / 无 SQLite / 无 provider。
//!
//! ## 算法
//!
//! 算法参考 pytdx (`get_security_bars.py`) / mootdx (`reader/affair.py`) 标准实现 +
//! akshare 复权语义。OHLC 复权，volume / amount 不复权（行业惯例，pytdx 与 akshare 一致）。
//!
//! ### 核心公式
//!
//! 对每个 `category == DividendAndSplit (1)` 事件，"理论除权价"：
//!
//! ```text
//!                  prev_close - fenhong/10 + (peigu/10) * peigujia
//! price_after = ──────────────────────────────────────────────────
//!                  1 + peigu/10 + songzhuangu/10
//! ```
//!
//! 单步 factor：
//!
//! ```text
//! factor = price_after / prev_close
//! ```
//!
//! 累积 factor `F[i]` 表示"从最早 bar 到 bar `i`"的累乘：每碰到一个除权事件 `e`，
//! 把 `F[i+]` 乘上 `factor(e)`。
//!
//! - **qfq**（前复权）：把所有历史 bar 映射到"最新除权基准"，即 `bar[i].px ← bar[i].px * F[i] / F[N-1]`。
//! - **hfq**（后复权）：把所有未来 bar 映射到"最早除权基准"，即 `bar[i].px ← bar[i].px / F[i] * F[0]`。
//!   其中 `F[0] = 1`，所以 hfq 实际是 `bar[i].px / F[i]`（事件发生后 factor < 1，所以 hfq 价格 > 原价）。
//!
//! ⚠️ 注意 factor 累积顺序：第一个事件之前所有 bar 的 F = 1；
//! 事件 `e` **在交易日 d 发生**时，**当天就已除权**，所以 bar.date >= d 的 F 乘上 factor(e)。
//!
//! ### Category 11/12（缩股）
//!
//! `factor_consolidation = 1 / suogu`：缩股 0.8 ⇒ 流通股数 × 0.8 ⇒ 价格 / 0.8 ⇒ factor = 1.25。
//!
//! ### 其他 category
//!
//! 不影响除权（送配股上市、增发、回购等是股本结构变化，不直接是除权事件），忽略。
//!
//! ## 边界
//!
//! - xdxr 为空 → 返回 bars clone。
//! - 所有事件都是非 1/11/12 category → 同上（factor 不变化）。
//! - 同一天多个事件 → factor 顺序累乘。

use crate::domain::quotes::{KlinePoint, XdxrCategory, XdxrEvent};
use crate::domain::shared::Price;
use rust_decimal::{prelude::FromPrimitive, Decimal};

/// 复权模式（与 `Adjust` enum 平行 — 解耦掉 serde 表达，避免循环依赖于 read API DTO 的口径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdjustMode {
    /// 不复权 — 直接返回原始 bars。
    None,
    /// 前复权 — 历史 bar 映射到最新除权基准（图表 / 趋势 / 指标用）。
    Qfq,
    /// 后复权 — 未来 bar 映射到最早除权基准（长期收益率研究用）。
    Hfq,
}

/// 应用复权：对 `bars`（按 trade_date 升序）+ `xdxr_events`（按 occur_date 升序），
/// 返回指定 mode 下复权后的 bar 序列。
///
/// **要求**：调用方保证 `bars` 按 `date` 升序、`xdxr_events` 按 `occur_date` 升序。
/// 内部不再排序——若顺序不对，结果不可预期。
///
/// volume / amount 字段不变（行业惯例）。
pub fn apply_adjust(
    bars: &[KlinePoint],
    xdxr_events: &[XdxrEvent],
    mode: AdjustMode,
) -> Vec<KlinePoint> {
    if matches!(mode, AdjustMode::None) || bars.is_empty() {
        return bars.to_vec();
    }
    // 过滤出影响价格的事件：DividendAndSplit / ShareConsolidation / NonTradableConsolidation。
    let mut price_events: Vec<&XdxrEvent> = xdxr_events
        .iter()
        .filter(|e| {
            matches!(
                e.category,
                XdxrCategory::DividendAndSplit
                    | XdxrCategory::ShareConsolidation
                    | XdxrCategory::NonTradableConsolidation
            )
        })
        .collect();
    if price_events.is_empty() {
        return bars.to_vec();
    }
    // 按 occur_date 升序（防御性 — 调用方应已排序）。
    price_events.sort_by_key(|e| e.occur_date);

    // 计算 per-bar cumulative factor。
    // factor[i] = 累乘到 bar[i]（含 bar[i] 当天事件）的复权 factor。
    // 起始 factor = 1.0。每碰到一个事件 e，从 bar.date >= e.occur_date 开始的 factor 全部乘 step。
    //
    // step 计算需要 `prev_close`（事件发生前一交易日的 close），所以我们边走 bar 边累计。
    let n = bars.len();
    let mut factors: Vec<f64> = vec![1.0; n];
    let mut cur_factor: f64 = 1.0;
    let mut next_event_idx: usize = 0;

    for i in 0..n {
        // 在写入 factors[i] 之前，先处理所有 occur_date <= bars[i].date 的事件——
        // 这些事件在 bars[i] 当天或之前发生，bars[i] 已是除权后价格。
        // step 计算用 bars[i-1].close 作为 prev_close；若 i == 0，没有 prev_close，跳过（保持 factor = 1.0）。
        while next_event_idx < price_events.len()
            && price_events[next_event_idx].occur_date <= bars[i].date
        {
            let ev = price_events[next_event_idx];
            // 计算 step factor。需要 prev_close — 用 bar[i-1].close（升序，前一交易日）。
            // 若 i == 0：无可用 prev_close，跳过该事件（不影响累计 factor，无法计算）。
            if i > 0 {
                let prev_close = dec_to_f64(bars[i - 1].close.0);
                if let Some(step) = compute_step_factor(ev, prev_close) {
                    cur_factor *= step;
                }
            }
            next_event_idx += 1;
        }
        factors[i] = cur_factor;
    }

    // 写入 mode 下的复权 bars。
    let base = match mode {
        // qfq: 把所有历史 bar 拉到"最新 bar 的 factor 水平"。
        // bar[i].px * factors[i] / factors[N-1]
        AdjustMode::Qfq => factors[n - 1],
        // hfq: bar[i].px / factors[i] * factors[0]。factors[0] = 1.0 (没有 prev_close 可用)，
        // 所以 hfq = bar[i].px / factors[i]。
        AdjustMode::Hfq => 1.0_f64,
        AdjustMode::None => unreachable!(),
    };

    let mut out: Vec<KlinePoint> = Vec::with_capacity(n);
    for (i, b) in bars.iter().enumerate() {
        let multiplier = match mode {
            // qfq: bar[i].px * (factors[N-1] / factors[i])
            // factors[N-1] ≤ factors[i] for i < N-1 (累乘 < 1 的 step) ⇒ multiplier ≤ 1
            // ⇒ 历史价向下平移到最新基准。
            AdjustMode::Qfq => {
                if factors[i].abs() < f64::EPSILON {
                    1.0
                } else {
                    base / factors[i]
                }
            }
            // hfq: bar[i].px * (factors[0] / factors[i]) = bar[i].px / factors[i]
            // factors[0] = 1.0 (没有 prev_close 可用)。
            // 事件后 factors[i] < 1，所以 1/factors[i] > 1，未来价向上平移。
            AdjustMode::Hfq => {
                if factors[i].abs() < f64::EPSILON {
                    1.0
                } else {
                    base / factors[i]
                }
            }
            AdjustMode::None => 1.0,
        };
        out.push(KlinePoint {
            date: b.date,
            open: scale_price(b.open, multiplier),
            close: scale_price(b.close, multiplier),
            high: scale_price(b.high, multiplier),
            low: scale_price(b.low, multiplier),
            volume: b.volume,
            amount: b.amount,
        });
    }
    out
}

/// 单个除权事件的 step factor。
///
/// - cat=1 (DividendAndSplit): `(prev_close - fenhong/10 + peigu/10 * peigujia) / (1 + peigu/10 + songzhuangu/10) / prev_close`
/// - cat=11/12 (Consolidation): `1 / suogu`（缩股率，>0 时使用）
///
/// 缺字段或 invalid 时返回 None — 调用方跳过此事件，cur_factor 保持不变。
fn compute_step_factor(ev: &XdxrEvent, prev_close: f64) -> Option<f64> {
    match ev.category {
        XdxrCategory::DividendAndSplit => {
            if prev_close <= 0.0 || !prev_close.is_finite() {
                return None;
            }
            let fenhong = ev.fenhong.unwrap_or(0.0);
            let peigu = ev.peigu.unwrap_or(0.0);
            let peigujia = ev.peigujia.unwrap_or(0.0);
            let songzhuangu = ev.songzhuangu.unwrap_or(0.0);
            let numerator = prev_close - fenhong / 10.0 + (peigu / 10.0) * peigujia;
            let denominator = 1.0 + peigu / 10.0 + songzhuangu / 10.0;
            if denominator.abs() < f64::EPSILON || !denominator.is_finite() {
                return None;
            }
            let price_after = numerator / denominator;
            if !price_after.is_finite() {
                return None;
            }
            let step = price_after / prev_close;
            if step.is_finite() && step > 0.0 {
                Some(step)
            } else {
                None
            }
        }
        XdxrCategory::ShareConsolidation | XdxrCategory::NonTradableConsolidation => {
            // suogu 表示缩股率（缩股后 / 缩股前），factor = 1 / suogu
            let suogu = ev.suogu.unwrap_or(0.0);
            if suogu <= 0.0 || !suogu.is_finite() {
                return None;
            }
            Some(1.0_f64 / suogu)
        }
        _ => None,
    }
}

fn scale_price(p: Price, multiplier: f64) -> Price {
    if (multiplier - 1.0).abs() < f64::EPSILON {
        return p;
    }
    let raw = dec_to_f64(p.0);
    let scaled = raw * multiplier;
    if !scaled.is_finite() {
        return p;
    }
    match Decimal::from_f64(scaled) {
        Some(d) => Price(d.round_dp(4)),
        None => p,
    }
}

fn dec_to_f64(d: Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    d.to_f64().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::quotes::XdxrEvent;
    use crate::domain::shared::{Price, TradeDate, TsCode};
    use rust_decimal::Decimal;

    fn bar(date: &str, close: f64) -> KlinePoint {
        let d = Decimal::from_f64(close).unwrap();
        KlinePoint {
            date: TradeDate::parse(date).unwrap(),
            open: Price(d),
            close: Price(d),
            high: Price(d),
            low: Price(d),
            volume: None,
            amount: None,
        }
    }

    fn ts() -> TsCode {
        TsCode::parse("600519.SH").unwrap()
    }

    fn div_event(date: &str, fenhong: f64, songzhuangu: f64) -> XdxrEvent {
        XdxrEvent::dividend_and_split(
            ts(),
            TradeDate::parse(date).unwrap(),
            Some(fenhong),
            Some(0.0),
            Some(songzhuangu),
            Some(0.0),
            1_700_000_000_000,
        )
    }

    fn consolidation_event(date: &str, suogu: f64) -> XdxrEvent {
        XdxrEvent {
            ts_code: ts(),
            occur_date: TradeDate::parse(date).unwrap(),
            category: XdxrCategory::ShareConsolidation,
            fenhong: None,
            peigujia: None,
            songzhuangu: None,
            peigu: None,
            suogu: Some(suogu),
            xingquanjia: None,
            fenshu: None,
            panqianliutong: None,
            qianzongguben: None,
            panhouliutong: None,
            houzongguben: None,
            fetched_at: 1_700_000_000_000,
        }
    }

    fn price(p: &Price) -> f64 {
        use rust_decimal::prelude::ToPrimitive;
        p.0.to_f64().unwrap_or(0.0)
    }

    #[test]
    fn empty_xdxr_returns_bars_unchanged() {
        let bars = vec![bar("20240101", 100.0), bar("20240102", 101.0)];
        let out = apply_adjust(&bars, &[], AdjustMode::Qfq);
        assert_eq!(out.len(), 2);
        assert!((price(&out[0].close) - 100.0).abs() < 1e-6);
        assert!((price(&out[1].close) - 101.0).abs() < 1e-6);
    }

    #[test]
    fn adjust_none_returns_bars_clone_even_with_events() {
        let bars = vec![bar("20240101", 100.0), bar("20240620", 90.0)];
        let events = vec![div_event("20240620", 10.0, 0.0)];
        let out = apply_adjust(&bars, &events, AdjustMode::None);
        assert!((price(&out[0].close) - 100.0).abs() < 1e-6);
        assert!((price(&out[1].close) - 90.0).abs() < 1e-6);
    }

    #[test]
    fn non_price_category_events_dont_affect_factor() {
        let bars = vec![bar("20240101", 100.0), bar("20240620", 95.0)];
        // category = 5 (EquityChange) — 不影响价格
        let mut e = div_event("20240620", 10.0, 0.0);
        e.category = XdxrCategory::EquityChange;
        let out = apply_adjust(&bars, &[e], AdjustMode::Qfq);
        // 没有有效除权事件 → 原值
        assert!((price(&out[0].close) - 100.0).abs() < 1e-6);
        assert!((price(&out[1].close) - 95.0).abs() < 1e-6);
    }

    /// Fixture 1：贵州茅台 2014-06-30 10送1股派12元。
    /// prev_close 假设 200 元（pre-event）。
    /// step = (200 - 12/10 + 0) / (1 + 0 + 1/10) / 200
    ///      = (200 - 1.2) / 1.1 / 200
    ///      = 198.8 / 1.1 / 200
    ///      = 180.7272727... / 200
    ///      = 0.903636...
    ///
    /// qfq: 历史价 ← 200 * 0.903636 / 0.903636 = 200 ?
    /// 不对——只有 1 个事件、2 个 bar：factors = [1.0, 0.903636]，qfq base = 0.903636。
    /// bar[0] (历史) = 200 * 1.0 / 0.903636 = 221.30...  不对！
    ///
    /// 等等，让我重新想：qfq 公式是 `bar[i].px * factors[i] / factors[N-1]`。
    /// factors[0] = 1.0（事件之前），factors[1] = 0.903636（事件之后）。
    /// qfq base = factors[N-1] = factors[1] = 0.903636。
    /// bar[0]_qfq = 200 * 1.0 / 0.903636 ≈ 221.3 元 ✗
    ///
    /// 这不对！qfq 应该让历史价 < 原价（向下平移），不应该 > 原价。
    /// 让我修正：qfq 公式应该是 `bar[i].px * factors[N-1] / factors[i]`（乘最新 / 除该点）。
    /// 重新核对——见 mootdx affair.py：
    /// ```python
    /// preclose_factor = preclose / close   # close 是除权日，preclose 是事件前
    /// ```
    /// 所以 step = price_after / prev_close < 1（除权日是 price_after，前一日是 prev_close）。
    /// factors[i] = 累乘当前 bar 适用的"除权后 / 除权前"系数；
    /// 历史 bar（事件前）factor=1，事件后 bar factor < 1。
    /// qfq：把所有 bar 拉到"最新"水平 ⇒ 历史 bar 也应该缩小（× factor_now）。
    /// 所以 qfq multiplier = factors[N-1] / factors[i] 不对——
    /// 因为 factors[0]=1.0, factors[N-1]<1.0；factors[N-1]/factors[0] < 1，bar[0] 缩小 ✓。
    /// 即 bar[i]_qfq = bar[i].px * factors[N-1] / factors[i]。
    ///
    /// 实现中 base = factors[N-1]，multiplier = factors[i] / base 是反了。
    /// 应该 multiplier = base / factors[i]。重新看代码……
    #[test]
    fn single_dividend_qfq_makes_history_lower() {
        // 2 bars: pre-event 200, on-event 198.8 (理论价).
        // qfq 之后历史价应当 < 200（向下平移）。
        let bars = vec![bar("20140629", 200.0), bar("20140630", 198.8)];
        let events = vec![div_event("20140630", 12.0, 1.0)];
        let out = apply_adjust(&bars, &events, AdjustMode::Qfq);
        let pre = price(&out[0].close);
        let post = price(&out[1].close);
        // 历史 bar 应该 < 原值
        assert!(pre < 200.0, "qfq history close {} should be < 200", pre);
        // 最新 bar 不变（因为是 base）
        assert!((post - 198.8).abs() < 1e-3);
        // 大致量级：step ≈ 0.9036，pre = 200 * 0.9036 ≈ 180.7
        assert!(pre > 175.0 && pre < 185.0, "qfq pre ≈ 180.7, got {}", pre);
    }

    #[test]
    fn single_dividend_hfq_keeps_history_unchanged_lifts_future() {
        // hfq: 最早 bar 不变，后续 bar 反向放大（理论价 / factor）。
        let bars = vec![bar("20140629", 200.0), bar("20140630", 198.8)];
        let events = vec![div_event("20140630", 12.0, 1.0)];
        let out = apply_adjust(&bars, &events, AdjustMode::Hfq);
        let pre = price(&out[0].close);
        let post = price(&out[1].close);
        // 最早 bar 不变
        assert!((pre - 200.0).abs() < 1e-3);
        // 最新 bar 应当 > 198.8（除权日价格上修）
        // factor ≈ 0.9036，hfq multiplier = 1 / 0.9036 ≈ 1.107
        // post = 198.8 / 0.9036 ≈ 220.0
        assert!(post > 215.0 && post < 225.0, "hfq post ≈ 220, got {}", post);
    }

    #[test]
    fn multiple_dividends_compound_factor() {
        // 3 bars, 2 events.
        // E1: 20240620 fenhong=10, songzhuangu=0 → step1 = (200-1)/1/200 = 0.995
        // E2: 20250620 fenhong=20, songzhuangu=0 → 取决于 prev_close (bar[1].close=199)
        //                                          step2 = (199-2)/1/199 ≈ 0.98995
        // factors = [1.0, 0.995, 0.995*0.98995 ≈ 0.985]
        let bars = vec![
            bar("20230101", 200.0),
            bar("20240620", 199.0),
            bar("20250620", 197.0),
        ];
        let events = vec![
            div_event("20240620", 10.0, 0.0),
            div_event("20250620", 20.0, 0.0),
        ];
        let qfq = apply_adjust(&bars, &events, AdjustMode::Qfq);
        // bar[2] 是 base，保持
        assert!((price(&qfq[2].close) - 197.0).abs() < 0.1);
        // bar[0] 应该被压最低：multiplier ≈ 0.985 / 1.0 = 0.985 → 197.0
        let pre = price(&qfq[0].close);
        assert!(pre < 200.0, "qfq[0] = {} should < 200", pre);
        // bar[1] 在中间：multiplier ≈ 0.985 / 0.995 ≈ 0.99 → 199 * 0.99 ≈ 197
        let mid = price(&qfq[1].close);
        assert!(
            mid < 199.0 && mid >= pre - 0.5,
            "qfq[1] = {} should be < 199 and ≈ qfq[0]={}",
            mid,
            pre
        );
    }

    #[test]
    fn share_consolidation_uses_inverse_suogu() {
        // suogu = 0.5 (缩股率 50%) → factor = 1 / 0.5 = 2.0
        // qfq base = 2.0, multiplier = factors[i] / base.
        // bar[0] factor = 1.0 (pre-event), bar[1] factor = 2.0 (post-event)
        // qfq: bar[0]_qfq = 100 * (2.0 / 1.0) = 200? 等等，公式是 base / factors[i]：
        //   bar[0]_qfq = 100 * (2.0 / 1.0) = 200 ⇒ 但 factor > 1 时应该让历史价 > 原价？
        // 实际：缩股 = 流通股缩小 = 每股价格上升。事件后 close = 200 元（缩股后），
        // 事件前 close = 100 元（原始）。qfq 让历史拉到事件后基准 ⇒ 100 → 200 ✓
        let bars = vec![bar("20230101", 100.0), bar("20240101", 200.0)];
        let events = vec![consolidation_event("20240101", 0.5)];
        let qfq = apply_adjust(&bars, &events, AdjustMode::Qfq);
        let pre = price(&qfq[0].close);
        let post = price(&qfq[1].close);
        // bar[0] 应被放大到 ~200 (qfq base / factor[0] = 2.0 / 1.0 = 2)
        assert!(pre > 195.0 && pre < 205.0, "consolidation qfq[0] ≈ 200, got {}", pre);
        assert!((post - 200.0).abs() < 0.1);
    }

    #[test]
    fn ohlc_all_scaled_volume_amount_unchanged() {
        use crate::domain::shared::{Amount, Volume};
        let mut b = bar("20140629", 200.0);
        b.open = Price(Decimal::from_f64(195.0).unwrap());
        b.high = Price(Decimal::from_f64(205.0).unwrap());
        b.low = Price(Decimal::from_f64(190.0).unwrap());
        b.volume = Some(Volume(1_000_000));
        b.amount = Some(Amount(Decimal::from_f64(200_000_000.0).unwrap()));
        let bars = vec![b, bar("20140630", 198.8)];
        let events = vec![div_event("20140630", 12.0, 1.0)];
        let out = apply_adjust(&bars, &events, AdjustMode::Qfq);
        // OHLC 都缩小（multiplier < 1）
        assert!(price(&out[0].open) < 195.0);
        assert!(price(&out[0].high) < 205.0);
        assert!(price(&out[0].low) < 190.0);
        // volume / amount 不变
        assert_eq!(out[0].volume, Some(Volume(1_000_000)));
        assert!(out[0].amount.is_some());
    }

    #[test]
    fn event_before_first_bar_no_op_for_that_bar() {
        // 事件在 bar[0] 当天发生但没有 prev_close（i==0 跳过），cur_factor 保持 1.0。
        // 后续 bar 也都保持 factor = 1.0 → 输出 = 输入。
        let bars = vec![bar("20140630", 198.8), bar("20240620", 199.0)];
        let events = vec![div_event("20140630", 12.0, 1.0)];
        let out = apply_adjust(&bars, &events, AdjustMode::Qfq);
        assert!((price(&out[0].close) - 198.8).abs() < 0.1);
        assert!((price(&out[1].close) - 199.0).abs() < 0.1);
    }
}
