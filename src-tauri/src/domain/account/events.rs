//! AccountEvent + AccountEventType。
//!
//! Spec: docs/design/account-module.md §2 (账户事件模型)

use crate::domain::shared::{JsonValue, OccurredAt, TsCode};
use serde::{Deserialize, Serialize};
use specta::Type;

/// Spec: account-module.md §2 AccountEventType。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum AccountEventType {
    AccountInitialized,
    OrderPlaced,
    OrderCancelled,
    OrderRejected,
    OrderExpired,
    OrderPartiallyFilled,
    OrderFilled,
    PositionOpened,
    PositionScaled,
    PositionClosed,
    ProtectionAdjusted,
    WatchlistAdded,
    WatchlistRemoved,
    WatchlistNoteUpdated,
    CashFrozen,
    CashReleased,
    SharesFrozen,
    SharesReleased,
    InvalidationSignalRecorded,
    TriggerCreated,
    TriggerHandled,
    SnapshotRebuilt,
}

impl AccountEventType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AccountInitialized => "account_initialized",
            Self::OrderPlaced => "order_placed",
            Self::OrderCancelled => "order_cancelled",
            Self::OrderRejected => "order_rejected",
            Self::OrderExpired => "order_expired",
            Self::OrderPartiallyFilled => "order_partially_filled",
            Self::OrderFilled => "order_filled",
            Self::PositionOpened => "position_opened",
            Self::PositionScaled => "position_scaled",
            Self::PositionClosed => "position_closed",
            Self::ProtectionAdjusted => "protection_adjusted",
            Self::WatchlistAdded => "watchlist_added",
            Self::WatchlistRemoved => "watchlist_removed",
            Self::WatchlistNoteUpdated => "watchlist_note_updated",
            Self::CashFrozen => "cash_frozen",
            Self::CashReleased => "cash_released",
            Self::SharesFrozen => "shares_frozen",
            Self::SharesReleased => "shares_released",
            Self::InvalidationSignalRecorded => "invalidation_signal_recorded",
            Self::TriggerCreated => "trigger_created",
            Self::TriggerHandled => "trigger_handled",
            Self::SnapshotRebuilt => "snapshot_rebuilt",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "account_initialized" => Self::AccountInitialized,
            "order_placed" => Self::OrderPlaced,
            "order_cancelled" => Self::OrderCancelled,
            "order_rejected" => Self::OrderRejected,
            "order_expired" => Self::OrderExpired,
            "order_partially_filled" => Self::OrderPartiallyFilled,
            "order_filled" => Self::OrderFilled,
            "position_opened" => Self::PositionOpened,
            "position_scaled" => Self::PositionScaled,
            "position_closed" => Self::PositionClosed,
            "protection_adjusted" => Self::ProtectionAdjusted,
            "watchlist_added" => Self::WatchlistAdded,
            "watchlist_removed" => Self::WatchlistRemoved,
            "watchlist_note_updated" => Self::WatchlistNoteUpdated,
            "cash_frozen" => Self::CashFrozen,
            "cash_released" => Self::CashReleased,
            "shares_frozen" => Self::SharesFrozen,
            "shares_released" => Self::SharesReleased,
            "invalidation_signal_recorded" => Self::InvalidationSignalRecorded,
            "trigger_created" => Self::TriggerCreated,
            "trigger_handled" => Self::TriggerHandled,
            "snapshot_rebuilt" => Self::SnapshotRebuilt,
            _ => return None,
        })
    }
}

/// Spec: account-module.md §2 AccountEvent。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AccountEvent {
    pub event_id: String,
    pub event_type: AccountEventType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts_code: Option<TsCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `agent` | `system` | `user`（AccountActor）。
    pub actor: String,
    pub payload: JsonValue,
    pub occurred_at: OccurredAt,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_type_roundtrip() {
        let kinds = [
            AccountEventType::AccountInitialized,
            AccountEventType::OrderPlaced,
            AccountEventType::OrderFilled,
            AccountEventType::OrderRejected,
            AccountEventType::OrderCancelled,
            AccountEventType::OrderExpired,
            AccountEventType::OrderPartiallyFilled,
            AccountEventType::PositionOpened,
            AccountEventType::PositionScaled,
            AccountEventType::PositionClosed,
            AccountEventType::ProtectionAdjusted,
            AccountEventType::WatchlistAdded,
            AccountEventType::WatchlistRemoved,
            AccountEventType::WatchlistNoteUpdated,
            AccountEventType::CashFrozen,
            AccountEventType::CashReleased,
            AccountEventType::SharesFrozen,
            AccountEventType::SharesReleased,
            AccountEventType::InvalidationSignalRecorded,
            AccountEventType::TriggerCreated,
            AccountEventType::TriggerHandled,
            AccountEventType::SnapshotRebuilt,
        ];
        for k in kinds {
            let s = k.as_str();
            assert_eq!(AccountEventType::from_str(s), Some(k));
        }
    }

    #[test]
    fn event_type_serialize_snake_case() {
        let t = AccountEventType::OrderFilled;
        let s = serde_json::to_string(&t).unwrap();
        assert_eq!(s, "\"order_filled\"");
    }

    #[test]
    fn unknown_event_type_returns_none() {
        assert!(AccountEventType::from_str("zzz").is_none());
    }
}
