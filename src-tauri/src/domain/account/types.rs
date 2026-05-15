//! Account domain 类型——Position / PositionEvent / AccountSnapshot 等。
//!
//! 关键设计选择：
//!
//! 1. **PositionStatus 富 enum** —— Closed 必带 exit_price/exit_at/reason；
//!    编译期排除"Closed 但缺退出信息"的不可能态。
//! 2. **PositionEventKind 富 enum** —— 每种事件的 payload 强类型，无 stringly-typed payload。
//! 3. **手续费 / 印花税在 event 上** —— 不是凭空算，而是事件发生时记录的事实，
//!    cash 派生时直接用，可审计。
//! 4. **首次入场价不在 Position 上** —— Position 只存 avg_entry_price（当前均价）；
//!    首次价从事件链找最早的 Opened event 派生，避免冗余。

use crate::domain::shared::{OccurredAt, Shares, StockCode, Yuan};
use serde::{Deserialize, Serialize};

// ============================================================================
// PositionId
// ============================================================================

/// UUID v4 形式的 position 唯一标识。
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

// ============================================================================
// Position
// ============================================================================

/// 模拟仓位——核心实体。
///
/// 字段语义：
/// - `avg_entry_price`：**当前均价**（无加仓时 = 首次价，加仓后按加权均价更新）
/// - `current_shares`：当前持有股数（满足 ≥ 100 + 100 倍数约束）
/// - `stop_loss` / `take_profit` / `time_stop_at`：**复盘契约**——agent 立的计划，
///   不会自动触发平仓；平仓时 caller 传入 `CloseReason` 复盘对比
/// - `thesis`：开仓时 agent 写下的判断理由
/// - `source_analysis_id`：关联 briefing 或 review 的 record id（来源审计）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub id: PositionId,
    pub code: StockCode,
    pub name: String,

    /// 当前均价（加仓后按加权均价更新；减仓不动）
    pub avg_entry_price: Yuan,
    /// 当前持有股数
    pub current_shares: Shares,

    pub status: PositionStatus,

    /// 复盘契约——agent 立的止损价
    pub stop_loss: Option<Yuan>,
    /// 复盘契约——agent 立的止盈价
    pub take_profit: Option<Yuan>,
    /// 复盘契约——agent 立的时间止损
    pub time_stop_at: Option<OccurredAt>,

    /// 开仓时 agent 写的判断理由
    pub thesis: String,
    /// 来源分析记录 id（briefing/review record）
    pub source_analysis_id: String,

    pub entered_at: OccurredAt,
}

/// 仓位状态——富 enum 编译期排除"Closed 但缺退出信息"的不可能态。
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

/// 平仓原因——复盘对比 agent 当初立的契约 vs 实际平仓动因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// 主动平仓（无特殊原因）
    Manual,
    /// 触发止损（agent 看到价格跌破 stop_loss 后决定平）
    StopLoss,
    /// 触发止盈（agent 看到价格涨到 take_profit 后决定平）
    TakeProfit,
    /// 时间止损（持仓周期到 time_stop_at 后决定平）
    TimeStop,
    /// 假设作废（review 复盘判断 thesis 不再成立）
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
}

// ============================================================================
// Side（买 / 卖）—— 涨跌停校验用
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

// ============================================================================
// PositionEvent —— append-only 审计真源
// ============================================================================

/// 仓位事件——事件链是真源，positions 状态 + cash 全部从事件派生。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionEvent {
    pub id: String,
    pub position_id: PositionId,
    pub kind: PositionEventKind,
    pub occurred_at: OccurredAt,
    pub source: EventSource,
    /// agent 在事件上的备注 markdown（briefing 简短理由 / review 复盘等）
    #[serde(default)]
    pub agent_note_md: String,
}

/// 事件 payload——强类型 enum，每种事件携带的字段不一样。
///
/// 注意：手续费 / 印花税都记录在事件上（不是凭空算）——这样 cash 派生用的就是
/// 事件发生时的事实，审计链 100% 完整。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PositionEventKind {
    /// 开仓
    Opened {
        entry_price: Yuan,
        shares: Shares,
        /// 佣金（0.025% 双向最低 5 元）
        commission: Yuan,
    },
    /// 加仓
    ScaledIn {
        /// 加仓股数
        delta: Shares,
        price: Yuan,
        /// 加仓后的新均价
        new_avg: Yuan,
        commission: Yuan,
    },
    /// 减仓（不归零；归零走 Closed）
    ScaledOut {
        /// 减仓股数（正数）
        delta: Shares,
        price: Yuan,
        commission: Yuan,
        /// 印花税（0.1%，仅卖出）
        stamp_tax: Yuan,
    },
    /// 平仓（清空所有持仓）
    Closed {
        exit_price: Yuan,
        /// 平仓时持有股数（等于平仓前的 current_shares）
        shares: Shares,
        reason: CloseReason,
        commission: Yuan,
        stamp_tax: Yuan,
    },
    /// 调止损 / 止盈 / 时间止损
    StopsAdjusted {
        stop_loss: Option<Yuan>,
        take_profit: Option<Yuan>,
        time_stop_at: Option<OccurredAt>,
    },
    /// 复盘记录，不影响现金和持仓数量。
    Reviewed {
        thesis_status: Option<String>,
        confidence: Option<f64>,
    },
    /// 止损 / 止盈 / 时间止损 / thesis invalidated 等触发信号。
    /// 这些信号只做审计，真正现金变化由后续 `Closed` 事件表达。
    Signal { signal: PositionSignalKind },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionSignalKind {
    StopTriggered,
    TakeProfitHit,
    TimeStopHit,
    Invalidated,
}

impl PositionEventKind {
    /// 事件类型 tag——给 repo 序列化和日志/事件审计用。
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Opened { .. } => "opened",
            Self::ScaledIn { .. } => "scaled_in",
            Self::ScaledOut { .. } => "scaled_out",
            Self::Closed { .. } => "closed",
            Self::StopsAdjusted { .. } => "stops_adjusted",
            Self::Reviewed { .. } => "reviewed",
            Self::Signal { signal } => signal.as_str(),
        }
    }
}

impl PositionSignalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StopTriggered => "stop_triggered",
            Self::TakeProfitHit => "take_profit_hit",
            Self::TimeStopHit => "time_stop_hit",
            Self::Invalidated => "invalidated",
        }
    }
}

/// 事件来源——审计链上"是谁触发的"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventSource {
    /// Briefing 流水线触发——关联 briefing analysis_id
    Briefing { analysis_id: String },
    /// Review 流水线触发——关联 review analysis_id
    Review { analysis_id: String },
    /// Chat 对话触发——关联 chat message_id
    Chat { message_id: String },
    /// 用户手动 / UI 触发
    Manual,
    /// 系统自动（reset / migration / 自动清理）
    System,
}

// ============================================================================
// AccountSnapshot —— 派生视图（in-memory cache）
// ============================================================================

/// 账户当前快照——从 positions + events + MARKET_SNAPSHOT 派生。
///
/// 字段：
/// - `cash`：现金 = initial_cash + Σ event_cash_delta（手续费 / 印花税都已扣）
/// - `market_value`：open positions × 当前价（来自 MARKET_SNAPSHOT）
/// - `realized_pnl`：已平仓 PnL（Σ closed positions 的 (exit-entry)×shares - 费）
/// - `unrealized_pnl`：浮盈浮亏（Σ open positions 的 (current_price-avg)×shares）
/// - `total_pnl` = realized_pnl + unrealized_pnl
/// - `total_assets` = cash + market_value
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSnapshot {
    pub initial_cash: Yuan,
    pub cash: Yuan,

    pub open_positions: Vec<Position>,
    pub closed_positions: Vec<Position>,

    pub market_value: Yuan,
    pub realized_pnl: Yuan,
    pub unrealized_pnl: Yuan,
    pub total_pnl: Yuan,
    pub total_assets: Yuan,

    pub captured_at: OccurredAt,
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_status_open_vs_closed() {
        let open = PositionStatus::Open;
        let closed = PositionStatus::Closed {
            exit_price: Yuan::new(12.0).unwrap(),
            exit_at: OccurredAt::now(),
            reason: CloseReason::TakeProfit,
        };
        assert!(open.is_open());
        assert!(!open.is_closed());
        assert!(closed.is_closed());
        assert!(!closed.is_open());
    }

    #[test]
    fn close_reason_as_str_stable() {
        assert_eq!(CloseReason::Manual.as_str(), "manual");
        assert_eq!(CloseReason::StopLoss.as_str(), "stop_loss");
        assert_eq!(CloseReason::TakeProfit.as_str(), "take_profit");
        assert_eq!(CloseReason::TimeStop.as_str(), "time_stop");
        assert_eq!(CloseReason::Invalidated.as_str(), "invalidated");
    }

    #[test]
    fn event_kind_tags() {
        let opened = PositionEventKind::Opened {
            entry_price: Yuan::new(11.5).unwrap(),
            shares: Shares::new(100).unwrap(),
            commission: Yuan::new(5.0).unwrap(),
        };
        assert_eq!(opened.tag(), "opened");

        let stops = PositionEventKind::StopsAdjusted {
            stop_loss: Some(Yuan::new(10.0).unwrap()),
            take_profit: None,
            time_stop_at: None,
        };
        assert_eq!(stops.tag(), "stops_adjusted");
    }

    #[test]
    fn position_id_unique() {
        let a = PositionId::new();
        let b = PositionId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn event_kind_serde_round_trip() {
        let kind = PositionEventKind::Opened {
            entry_price: Yuan::new(11.5).unwrap(),
            shares: Shares::new(200).unwrap(),
            commission: Yuan::new(5.0).unwrap(),
        };
        let json = serde_json::to_string(&kind).unwrap();
        assert!(json.contains("\"kind\":\"opened\""));
        let back: PositionEventKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, back);
    }

    #[test]
    fn event_source_serde_round_trip() {
        let src = EventSource::Briefing {
            analysis_id: "abc-123".into(),
        };
        let json = serde_json::to_string(&src).unwrap();
        assert!(json.contains("\"kind\":\"briefing\""));
        // 字段 snake_case（默认）—— 与 tag 风格一致
        assert!(json.contains("\"analysis_id\":\"abc-123\""));
        let back: EventSource = serde_json::from_str(&json).unwrap();
        assert_eq!(src, back);
    }

    #[test]
    fn position_status_serde() {
        let closed = PositionStatus::Closed {
            exit_price: Yuan::new(12.5).unwrap(),
            exit_at: OccurredAt::new(1747200000000),
            reason: CloseReason::TakeProfit,
        };
        let json = serde_json::to_string(&closed).unwrap();
        assert!(json.contains("\"state\":\"closed\""));
        let back: PositionStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(closed, back);
    }
}
