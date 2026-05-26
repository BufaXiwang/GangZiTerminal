//! `AccountEvent` —— spec `account-module.md §2 账户事件模型`。
//!
//! Account 状态变化真源：订单 / 成交 / 仓位 / lot / 保护条件 / 自选 / cash 冻结
//! 释放 / 触发器 / snapshot 重建。每个状态变化必须先写一条 `AccountEvent`，
//! 再更新派生读模型；snapshot 可由事件流 + 当前行情完整重建。
//!
//! 这是一条 append-only 审计流。`PositionEvent`（domain/account/events.rs）是
//! 历史遗留的 position-only 子流，会逐步迁移到本统一流上；当前阶段两者并存：
//! 业务写动作同时产出 `PositionEvent`（保持现有 snapshot 派生路径）+
//! `AccountEvent`（spec 对外契约）。

use serde::{Deserialize, Serialize};

use super::events::AccountActor;

/// spec `AccountEventType` 闭集合（21 值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
            AccountEventType::AccountInitialized => "account_initialized",
            AccountEventType::OrderPlaced => "order_placed",
            AccountEventType::OrderCancelled => "order_cancelled",
            AccountEventType::OrderRejected => "order_rejected",
            AccountEventType::OrderExpired => "order_expired",
            AccountEventType::OrderPartiallyFilled => "order_partially_filled",
            AccountEventType::OrderFilled => "order_filled",
            AccountEventType::PositionOpened => "position_opened",
            AccountEventType::PositionScaled => "position_scaled",
            AccountEventType::PositionClosed => "position_closed",
            AccountEventType::ProtectionAdjusted => "protection_adjusted",
            AccountEventType::WatchlistAdded => "watchlist_added",
            AccountEventType::WatchlistRemoved => "watchlist_removed",
            AccountEventType::WatchlistNoteUpdated => "watchlist_note_updated",
            AccountEventType::CashFrozen => "cash_frozen",
            AccountEventType::CashReleased => "cash_released",
            AccountEventType::SharesFrozen => "shares_frozen",
            AccountEventType::SharesReleased => "shares_released",
            AccountEventType::InvalidationSignalRecorded => "invalidation_signal_recorded",
            AccountEventType::TriggerCreated => "trigger_created",
            AccountEventType::TriggerHandled => "trigger_handled",
            AccountEventType::SnapshotRebuilt => "snapshot_rebuilt",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
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

/// 一条账户事件。spec `account-module.md §2`。
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub ts_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub actor: AccountActor,
    pub payload: serde_json::Value,
    pub occurred_at: String,
}

impl AccountEvent {
    pub fn new(
        event_type: AccountEventType,
        actor: AccountActor,
        payload: serde_json::Value,
    ) -> Self {
        let occurred_at = chrono::Utc::now().to_rfc3339();
        Self {
            event_id: format!("acc_evt_{}", uuid::Uuid::new_v4().simple()),
            event_type,
            order_id: None,
            fill_id: None,
            position_id: None,
            ts_code: None,
            reason: None,
            actor,
            payload,
            occurred_at,
        }
    }

    pub fn with_order(mut self, order_id: impl Into<String>) -> Self {
        self.order_id = Some(order_id.into());
        self
    }

    pub fn with_position(mut self, position_id: impl Into<String>) -> Self {
        self.position_id = Some(position_id.into());
        self
    }

    pub fn with_ts_code(mut self, ts_code: impl Into<String>) -> Self {
        self.ts_code = Some(ts_code.into());
        self
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        let r = reason.into();
        if !r.is_empty() {
            self.reason = Some(r);
        }
        self
    }

    pub fn with_fill(mut self, fill_id: impl Into<String>) -> Self {
        self.fill_id = Some(fill_id.into());
        self
    }
}
