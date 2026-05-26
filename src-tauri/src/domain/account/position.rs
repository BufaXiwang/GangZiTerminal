//! Position entity——模拟账户持仓 + 投资判断（v4 expectation-merged）。
//!
//! v4 合并 Expectation 后，Position 一肩挑两件事：
//! - **执行**：shares / avg_cost / status / stops——传统持仓字段
//! - **假设**：direction / invalidation_signals / signals_used / reasoning——原 Expectation 字段
//!
//! `PositionKind` 区分：
//! - `Live`：真持仓，shares > 0，扣现金，参与 PnL / valuation
//! - `Watch`：观察型（"看好但不买"），shares = 0，不动现金，只走 judge → close → lesson
//!
//! 自动平仓：scheduler tick 跑 `judge_position` → 触发条件命中（价格 / 信号 / 时间）→
//! `AccountService::close_position` 推进状态 + 写 Lesson + 反向打标 heuristic。
//! agent 不参与触发，只在主动撤回时调 `close_position(reason=Manual)`。

use crate::domain::shared::signal::SignalKind;
use crate::domain::shared::{OccurredAt, Shares, StockCode, Yuan};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PositionId(String);

impl PositionId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn from_string(s: String) -> Self {
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PositionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for PositionId {
    fn default() -> Self {
        Self::new()
    }
}

/// 持仓性质——区分真持仓和"看好但不买"的观察型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionKind {
    /// 真持仓：shares > 0，扣现金，参与 PnL / valuation
    Live,
    /// 观察型："看好但不买"，shares = 0，不动现金，只走 judge → close → lesson
    Watch,
}

impl PositionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Watch => "watch",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "live" => Some(Self::Live),
            "watch" => Some(Self::Watch),
            _ => None,
        }
    }
}

impl Default for PositionKind {
    fn default() -> Self {
        Self::Live
    }
}

/// 判断方向。RangeBound 在 v4 砍掉——实战极少用，需要时再加。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// 看涨：到/破 take_profit 上方
    Up,
    /// 看跌：到/破 stop_loss 下方（一般 Watch 用，Live 倒挂语义在 v4 简化）
    Down,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            _ => None,
        }
    }
}

impl Default for Direction {
    fn default() -> Self {
        Self::Up
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub id: PositionId,
    pub code: StockCode,
    pub name: String,
    /// Live = 真持仓；Watch = 观察型（shares=0，不动现金）
    #[serde(default)]
    pub kind: PositionKind,
    pub avg_entry_price: Yuan,
    pub current_shares: Shares,
    pub status: PositionStatus,
    pub stop_loss: Option<Yuan>,
    pub take_profit: Option<Yuan>,
    pub time_stop_at: Option<OccurredAt>,
    /// 看涨 / 看跌方向——决定 take_profit / stop_loss 的语义
    #[serde(default)]
    pub direction: Direction,
    /// 失效条件信号：review 时若任一 family 命中，提前判 Invalidated（不等价格止损）
    #[serde(default)]
    pub invalidation_signals: Vec<SignalKind>,
    /// 触发本次建仓的结构化信号列表——close 时反向打标 heuristic
    #[serde(default)]
    pub signals_used: Vec<SignalKind>,
    /// 自然语言决策上下文——叙事 / 为什么押。替代旧 thesis 字段，无长度限制。
    #[serde(default)]
    pub reasoning: String,
    pub source_analysis_id: String,
    /// 首次开仓时间——审计 / UI 展示用。spec 字段名 `openedAt`。
    #[serde(alias = "openedAt", rename(serialize = "openedAt"))]
    pub entered_at: OccurredAt,
    /// **最近一次买入时间**（Opened 或 ScaledIn 都更新）——T+1 判定基准。
    ///
    /// 为什么不直接用 `entered_at`：用户昨天 open + 今天 ScaledIn 后，`entered_at`
    /// 仍是昨天，但**今天买的那部分股票今天不能卖**。T+1 必须看最近一次买入。
    pub last_acquisition_at: OccurredAt,
    // ============ spec §2 派生字段（由 compute_snapshot 填充） ============
    /// spec `Position.sellableQuantity`：可卖数量（T+1 + 冻结派生）。
    /// PositionLot 模型完整接入前先用 `current_shares` 作 baseline 近似。
    #[serde(default)]
    pub sellable_quantity: i64,
    /// spec `Position.marketPrice`：来自 Quotes snapshot；缺行情时 None。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub market_price: Option<f64>,
    /// spec `Position.marketValue` = `marketPrice * quantity`。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub market_value: Option<f64>,
    /// spec `Position.unrealizedPnl` = `(marketPrice - avgCost) * quantity`。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub unrealized_pnl: Option<f64>,
    /// spec `Position.quoteFreshness`：估值行情新鲜度。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub quote_freshness: Option<crate::domain::shared::Freshness>,
    /// spec `account-module.md §2 Position.warnings: WarningCode[]`。
    /// 例如缺行情时返回 `quote_missing`、stale 行情 `quote_stale`。空表示无 warning。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<crate::domain::shared::WarningCode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PositionStatus {
    Open,
    Closed {
        exit_price: Yuan,
        exit_at: OccurredAt,
        reason: CloseReason,
    },
}

impl PositionStatus {
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open)
    }

    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    Manual,
    /// 价格止损（current ≤ stop_loss）
    StopLoss,
    /// 价格止盈（current ≥ take_profit）—— 等价于"假设命中"
    TakeProfit,
    /// 时间到期（current_time ≥ time_stop_at）——可能是 partial_hit（方向对未达目标）
    TimeStop,
    /// 失效信号触发（invalidation_signals 任一命中）—— 等价于"假设破"
    Invalidated,
}

impl CloseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::StopLoss => "stop_loss",
            Self::TakeProfit => "take_profit",
            Self::TimeStop => "time_stop",
            Self::Invalidated => "invalidated",
        }
    }

    /// 给 heuristic 反向打标用——是否计入命中/错过统计。
    ///
    /// - `Some(true)`：TakeProfit → 假设命中
    /// - `Some(false)`：StopLoss / Invalidated → 假设破
    /// - `None`：Manual（agent 主观撤）/ TimeStop（中性，可能 partial_hit）
    pub fn counts_for_signal_outcome(self) -> Option<bool> {
        match self {
            Self::TakeProfit => Some(true),
            Self::StopLoss | Self::Invalidated => Some(false),
            Self::Manual | Self::TimeStop => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

// ====== judge_position 纯函数 =========================================
//
// 等价于旧 expectation::judge_outcome；scheduler tick / 盘后跑。
// 不修改 position；仅返回应触发的 CloseReason（如有）。

/// review 判定结果——告诉调用方应该用哪个 CloseReason 平仓，或继续 pending。
#[derive(Debug, Clone, PartialEq)]
pub enum PositionOutcome {
    /// 继续持有，无触发
    StillOpen,
    /// 应平仓（带建议的 reason 和触发说明）
    ShouldClose { reason: CloseReason, note: String },
}

/// 纯函数判定：给定 position + 当前价 + 当前时间，应该触发哪种平仓。
///
/// 触发优先级（依次检查）：
///   1. invalidation_signals 命中 → Invalidated（外部 helper 先查 signal_detections 再传 `invalidation_hit`）
///   2. take_profit 命中（看涨：current ≥ tp；看跌：current ≤ tp）→ TakeProfit
///   3. stop_loss 命中（看涨：current ≤ sl；看跌：current ≥ sl）→ StopLoss
///   4. time_stop_at 到期 → TimeStop
///   5. 否则 StillOpen
///
/// `invalidation_hit` 由 scheduler 提前查 `signal_detections` 表后传入——
/// 让此函数保持纯（无 I/O）。
pub fn judge_position(
    pos: &Position,
    current_price: Yuan,
    now: OccurredAt,
    invalidation_hit: Option<String>,
) -> PositionOutcome {
    if !pos.status.is_open() {
        return PositionOutcome::StillOpen;
    }

    if let Some(family) = invalidation_hit {
        return PositionOutcome::ShouldClose {
            reason: CloseReason::Invalidated,
            note: format!("invalidation_signal 命中：family={family}"),
        };
    }

    let cur = current_price.value();

    if let Some(tp) = pos.take_profit {
        let hit = match pos.direction {
            Direction::Up => cur >= tp.value(),
            Direction::Down => cur <= tp.value(),
        };
        if hit {
            return PositionOutcome::ShouldClose {
                reason: CloseReason::TakeProfit,
                note: format!(
                    "{} take_profit 命中：current {:.4} vs target {:.4}",
                    pos.direction.as_str(),
                    cur,
                    tp.value()
                ),
            };
        }
    }

    if let Some(sl) = pos.stop_loss {
        let hit = match pos.direction {
            Direction::Up => cur <= sl.value(),
            Direction::Down => cur >= sl.value(),
        };
        if hit {
            return PositionOutcome::ShouldClose {
                reason: CloseReason::StopLoss,
                note: format!(
                    "{} stop_loss 命中：current {:.4} vs stop {:.4}",
                    pos.direction.as_str(),
                    cur,
                    sl.value()
                ),
            };
        }
    }

    if let Some(ts) = pos.time_stop_at {
        if now.value() >= ts.value() {
            return PositionOutcome::ShouldClose {
                reason: CloseReason::TimeStop,
                note: format!("time_stop 到期：now {} ≥ stop_at {}", now.value(), ts.value()),
            };
        }
    }

    PositionOutcome::StillOpen
}

/// PartialHit 派生判定——在 close(TimeStop) 后由 reflection 用。
///
/// "方向对但目标定高了"：close 时 PnL 方向与 direction 一致但未达 take_profit 目标涨幅。
/// 给 heuristic 计中性证据（既不算 hit 也不算 miss）。
///
/// 返回 `true` = partial_hit（中性），`false` = 完全 miss（计 miss）。
pub fn is_partial_hit(
    direction: Direction,
    avg_entry: Yuan,
    exit_price: Yuan,
    take_profit: Option<Yuan>,
) -> bool {
    let entry = avg_entry.value();
    let exit = exit_price.value();
    let tp = match take_profit {
        Some(v) => v.value(),
        None => return false, // 无目标价，无 partial 可言
    };
    if entry.abs() < f64::EPSILON {
        return false;
    }
    let actual_pct = match direction {
        Direction::Up => (exit - entry) / entry,
        Direction::Down => (entry - exit) / entry,
    };
    let target_pct = match direction {
        Direction::Up => (tp - entry) / entry,
        Direction::Down => (entry - tp) / entry,
    };
    actual_pct > 0.0 && actual_pct < target_pct
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(direction: Direction, tp: Option<f64>, sl: Option<f64>, ts: Option<i64>) -> Position {
        Position {
            id: PositionId::new(),
            code: StockCode::new("600519").unwrap(),
            name: "test".into(),
            kind: PositionKind::Live,
            avg_entry_price: Yuan::new(100.0).unwrap(),
            current_shares: Shares::from_unchecked(100),
            status: PositionStatus::Open,
            stop_loss: sl.map(|v| Yuan::new(v).unwrap()),
            take_profit: tp.map(|v| Yuan::new(v).unwrap()),
            time_stop_at: ts.map(OccurredAt::new),
            direction,
            invalidation_signals: vec![],
            signals_used: vec![],
            reasoning: "test".into(),
            source_analysis_id: "".into(),
            entered_at: OccurredAt::new(1_700_000_000_000),
            last_acquisition_at: OccurredAt::new(1_700_000_000_000),
            sellable_quantity: 0,
            market_price: None,
            market_value: None,
            unrealized_pnl: None,
            quote_freshness: None,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn judge_up_take_profit_hit() {
        let p = mk(Direction::Up, Some(110.0), Some(95.0), None);
        let out = judge_position(&p, Yuan::new(112.0).unwrap(), OccurredAt::new(1_700_000_001_000), None);
        assert!(matches!(out, PositionOutcome::ShouldClose { reason: CloseReason::TakeProfit, .. }));
    }

    #[test]
    fn judge_up_stop_loss_hit() {
        let p = mk(Direction::Up, Some(110.0), Some(95.0), None);
        let out = judge_position(&p, Yuan::new(94.0).unwrap(), OccurredAt::new(1_700_000_001_000), None);
        assert!(matches!(out, PositionOutcome::ShouldClose { reason: CloseReason::StopLoss, .. }));
    }

    #[test]
    fn judge_time_stop_priority_after_price() {
        let p = mk(Direction::Up, Some(110.0), Some(95.0), Some(1_700_000_000_500));
        // 价格未触发但时间到了
        let out = judge_position(&p, Yuan::new(100.0).unwrap(), OccurredAt::new(1_700_000_001_000), None);
        assert!(matches!(out, PositionOutcome::ShouldClose { reason: CloseReason::TimeStop, .. }));
    }

    #[test]
    fn judge_invalidation_signal_first() {
        let p = mk(Direction::Up, Some(110.0), Some(95.0), Some(1_700_000_001_000));
        let out = judge_position(
            &p,
            Yuan::new(100.0).unwrap(),
            OccurredAt::new(1_700_000_000_500),
            Some("BreakoutBelow20MA".into()),
        );
        match out {
            PositionOutcome::ShouldClose { reason: CloseReason::Invalidated, .. } => {}
            other => panic!("expected Invalidated, got {other:?}"),
        }
    }

    #[test]
    fn judge_still_open_when_nothing_triggered() {
        let p = mk(Direction::Up, Some(110.0), Some(95.0), Some(1_700_000_100_000));
        let out = judge_position(&p, Yuan::new(100.0).unwrap(), OccurredAt::new(1_700_000_001_000), None);
        assert_eq!(out, PositionOutcome::StillOpen);
    }

    #[test]
    fn partial_hit_when_direction_correct_but_short_of_target() {
        // entry=100, target=110, exit=105 → +5% < +10% target → partial
        assert!(is_partial_hit(
            Direction::Up,
            Yuan::new(100.0).unwrap(),
            Yuan::new(105.0).unwrap(),
            Some(Yuan::new(110.0).unwrap())
        ));
    }

    #[test]
    fn partial_hit_false_when_direction_wrong() {
        // entry=100, target=110, exit=95 → reverse → not partial
        assert!(!is_partial_hit(
            Direction::Up,
            Yuan::new(100.0).unwrap(),
            Yuan::new(95.0).unwrap(),
            Some(Yuan::new(110.0).unwrap())
        ));
    }

    #[test]
    fn close_reason_counts_signal_outcome() {
        assert_eq!(CloseReason::TakeProfit.counts_for_signal_outcome(), Some(true));
        assert_eq!(CloseReason::StopLoss.counts_for_signal_outcome(), Some(false));
        assert_eq!(CloseReason::Invalidated.counts_for_signal_outcome(), Some(false));
        assert_eq!(CloseReason::TimeStop.counts_for_signal_outcome(), None);
        assert_eq!(CloseReason::Manual.counts_for_signal_outcome(), None);
    }
}
