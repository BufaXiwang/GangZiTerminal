//! Position entity and position identity.

use crate::domain::account::thesis::ThesisId;
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub id: PositionId,
    pub code: StockCode,
    pub name: String,
    pub avg_entry_price: Yuan,
    pub current_shares: Shares,
    pub status: PositionStatus,
    pub stop_loss: Option<Yuan>,
    pub take_profit: Option<Yuan>,
    pub time_stop_at: Option<OccurredAt>,
    /// 简短论点摘要（≤120 字）—— v2 重构后只是 thesis 的展示用副本，
    /// 真正的"为什么"在 thesis_id 引用的 Thesis aggregate 里。
    pub thesis: String,
    /// 关联的 Thesis 聚合根 id（v2 新增）。
    /// `None` 表示这是用户直接命令建仓没绑 thesis 的特殊情况；agent 主动建仓必须设。
    #[serde(default)]
    pub thesis_id: Option<ThesisId>,
    pub source_analysis_id: String,
    /// 首次开仓时间——审计 / UI 展示用。
    pub entered_at: OccurredAt,
    /// **最近一次买入时间**（Opened 或 ScaledIn 都更新）——T+1 判定基准。
    ///
    /// 为什么不直接用 `entered_at`：用户昨天 open + 今天 ScaledIn 后，`entered_at`
    /// 仍是昨天，但**今天买的那部分股票今天不能卖**。T+1 必须看最近一次买入。
    ///
    /// 注：当前模型按"整仓口径"——任何一次今天买入都让整仓今天不能卖。比 FIFO
    /// per-lot 严格，但建模简单；agent 在策略里不会主动踩这条规则。
    pub last_acquisition_at: OccurredAt,
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
    StopLoss,
    TakeProfit,
    TimeStop,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}
