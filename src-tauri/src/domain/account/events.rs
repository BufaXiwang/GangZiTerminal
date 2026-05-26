//! Account event stream types.

use super::position::{CloseReason, PositionId};
use crate::domain::shared::{OccurredAt, Shares, Yuan};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionEvent {
    pub id: String,
    pub position_id: PositionId,
    pub kind: PositionEventKind,
    pub occurred_at: OccurredAt,
    pub source: EventSource,
    #[serde(default)]
    pub agent_note_md: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PositionEventKind {
    Opened {
        entry_price: Yuan,
        shares: Shares,
        commission: Yuan,
    },
    ScaledIn {
        delta: Shares,
        price: Yuan,
        new_avg: Yuan,
        commission: Yuan,
    },
    ScaledOut {
        delta: Shares,
        price: Yuan,
        commission: Yuan,
        stamp_tax: Yuan,
    },
    Closed {
        exit_price: Yuan,
        shares: Shares,
        reason: CloseReason,
        commission: Yuan,
        stamp_tax: Yuan,
    },
    StopsAdjusted {
        stop_loss: Option<Yuan>,
        take_profit: Option<Yuan>,
        time_stop_at: Option<OccurredAt>,
    },
    Signal {
        signal: PositionSignalKind,
    },
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
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Opened { .. } => "opened",
            Self::ScaledIn { .. } => "scaled_in",
            Self::ScaledOut { .. } => "scaled_out",
            Self::Closed { .. } => "closed",
            Self::StopsAdjusted { .. } => "stops_adjusted",
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventSource {
    /// 用户 chat 触发的写动作（含用户指令 + agent 主动判断）
    Chat { message_id: String },
    /// 收盘 reflection tick 触发（如 thesis 标记 invalidated 后自动平仓）
    Reflection { episode_id: String },
    Manual,
    System,
}

/// `AccountActor` —— spec `account-module.md §2`。
///
/// 跨模块对外的"账户写动作发起者"统一三元组：
/// - `agent`：Agent / 外部自动化决策方调用 Account 写入口
/// - `system`：系统内部维护（订单过期、成交评估、snapshot 重建、初始化）
/// - `user`：用户发起的非交易账户维护动作（仅 watchlist add / remove / update_note）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountActor {
    Agent,
    System,
    User,
}

impl AccountActor {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountActor::Agent => "agent",
            AccountActor::System => "system",
            AccountActor::User => "user",
        }
    }
}

/// 内部 `EventSource` → 对外 `AccountActor` 投影。
///
/// `Chat` / `Reflection` / `Manual` 都映射为 `agent`（前者是 agent 通过 chat
/// 操盘，后者是 agent 反向触发的写）；`System` 维持为 `system`。
impl EventSource {
    pub fn actor(&self) -> AccountActor {
        match self {
            EventSource::Chat { .. } => AccountActor::Agent,
            EventSource::Reflection { .. } => AccountActor::Agent,
            EventSource::Manual => AccountActor::Agent,
            EventSource::System => AccountActor::System,
        }
    }
}
