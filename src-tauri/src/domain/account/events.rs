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
    Reviewed {
        thesis_status: Option<String>,
        confidence: Option<f64>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventSource {
    Briefing { analysis_id: String },
    Review { analysis_id: String },
    Chat { message_id: String },
    Manual,
    System,
}
