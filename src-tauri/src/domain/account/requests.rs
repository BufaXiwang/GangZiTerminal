//! Account 对外 DTO — operate_account / update_watchlist / fetch_account / trigger handling。
//!
//! Spec: docs/design/account-module.md §4 (对外接口)

use crate::domain::account::events::AccountEvent;
use crate::domain::account::triggers::AccountTrigger;
use crate::domain::account::types::{
    AccountSnapshot, Order, OrderStatus, Position, WatchlistItem, WatchlistItemView,
};
use crate::domain::shared::{ErrorCode, OccurredAt, Price, Shares, TsCode, WarningCode};
use serde::{Deserialize, Deserializer, Serialize};
use specta::Type;

/// 区分 “缺省 (None) / 显式 null (Some(None)) / 显式 value (Some(Some))”。
///
/// Spec: account-module.md §4 adjust_protection — `null` 清除，缺省不修改。
fn deserialize_double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Ok(Some(Option::deserialize(de)?))
}

// ----------------------------------------------------------------------------
// AccountActor — 写入口允许的发起者集合。
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum AccountActor {
    Agent,
    System,
    User,
}

impl AccountActor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::System => "system",
            Self::User => "user",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "agent" => Self::Agent,
            "system" => Self::System,
            "user" => Self::User,
            _ => return None,
        })
    }
}

// ----------------------------------------------------------------------------
// fetch_account
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum PositionStatusFilter {
    Open,
    Closed,
    All,
}

impl Default for PositionStatusFilter {
    fn default() -> Self {
        Self::Open
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(untagged)]
pub enum TriggerHandledFilter {
    Bool(bool),
    All(TriggerHandledAll),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum TriggerHandledAll {
    All,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct OrderActiveFilter(pub bool);

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FetchAccountInclude {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub positions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orders: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watchlist: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triggers: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FetchAccountRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<FetchAccountInclude>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_status: Option<PositionStatusFilter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_status_in: Option<Vec<OrderStatus>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_handled: Option<TriggerHandledFilter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FetchAccountResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<AccountSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub positions: Option<Vec<Position>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orders: Option<Vec<Order>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watchlist: Option<Vec<WatchlistItemView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<AccountEvent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triggers: Option<Vec<AccountTrigger>>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<WarningCode>,
}

// ----------------------------------------------------------------------------
// operate_account
// ----------------------------------------------------------------------------

/// Spec: account-module.md §4 operate_account
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(tag = "action", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum OperateAccountAction {
    PlaceOrder {
        ts_code: TsCode,
        side: super::types::OrderSide,
        order_type: super::types::OrderType,
        #[serde(skip_serializing_if = "Option::is_none")]
        limit_price: Option<Price>,
        quantity: Shares,
        #[serde(skip_serializing_if = "Option::is_none")]
        expires_at: Option<OccurredAt>,
        reason: String,
    },
    CancelOrder {
        order_id: String,
        reason: String,
    },
    OpenPosition {
        ts_code: TsCode,
        quantity: Shares,
        #[serde(skip_serializing_if = "Option::is_none")]
        order_type: Option<super::types::OrderType>,
        #[serde(skip_serializing_if = "Option::is_none")]
        limit_price: Option<Price>,
        #[serde(skip_serializing_if = "Option::is_none")]
        expires_at: Option<OccurredAt>,
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_loss: Option<Price>,
        #[serde(skip_serializing_if = "Option::is_none")]
        take_profit: Option<Price>,
        #[serde(skip_serializing_if = "Option::is_none")]
        time_stop_at: Option<OccurredAt>,
        reason: String,
    },
    ScalePosition {
        position_id: String,
        side: ScaleSide,
        quantity: Shares,
        #[serde(skip_serializing_if = "Option::is_none")]
        order_type: Option<super::types::OrderType>,
        #[serde(skip_serializing_if = "Option::is_none")]
        limit_price: Option<Price>,
        #[serde(skip_serializing_if = "Option::is_none")]
        expires_at: Option<OccurredAt>,
        reason: String,
    },
    ClosePosition {
        position_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        quantity: Option<Shares>,
        #[serde(skip_serializing_if = "Option::is_none")]
        order_type: Option<super::types::OrderType>,
        #[serde(skip_serializing_if = "Option::is_none")]
        limit_price: Option<Price>,
        #[serde(skip_serializing_if = "Option::is_none")]
        expires_at: Option<OccurredAt>,
        reason: String,
    },
    AdjustProtection {
        position_id: String,
        /// `None` = 不修改；`Some(Some(price))` = 设置；`Some(None)` = 清除。
        ///
        /// 序列化形态：字段缺省 → 不修改；显式 `null` → 清除；显式 value → 设置。
        #[serde(default, deserialize_with = "deserialize_double_option")]
        stop_loss: Option<Option<Price>>,
        #[serde(default, deserialize_with = "deserialize_double_option")]
        take_profit: Option<Option<Price>>,
        #[serde(default, deserialize_with = "deserialize_double_option")]
        time_stop_at: Option<Option<OccurredAt>>,
        /// 全量替换语义（spec §2）：缺省 = 不修改；空数组 = 清空。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        invalidation_signals: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
        reason: String,
    },
    RecordInvalidationSignal {
        position_id: String,
        signal: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        evidence_ref: Option<String>,
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum ScaleSide {
    Increase,
    Decrease,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct OperateAccountRequest {
    #[serde(flatten)]
    pub action: OperateAccountAction,
}

/// Spec: account-module.md §4 OperateAccountResponse
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct OperateAccountResponse {
    pub accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<ErrorCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub fill_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection_event_id: Option<String>,
    pub account_event_ids: Vec<String>,
    pub snapshot: AccountSnapshot,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<WarningCode>,
}

// ----------------------------------------------------------------------------
// update_watchlist
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(tag = "action", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum UpdateWatchlistAction {
    Add {
        ts_code: TsCode,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Remove {
        ts_code: TsCode,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    UpdateNote {
        ts_code: TsCode,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct UpdateWatchlistRequest {
    #[serde(flatten)]
    pub action: UpdateWatchlistAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct UpdateWatchlistResponse {
    pub accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<ErrorCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<WatchlistItem>,
    pub account_event_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<WarningCode>,
}

// ----------------------------------------------------------------------------
// mark_trigger_handled
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct MarkTriggerHandledRequest {
    pub trigger_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct MarkTriggerHandledResponse {
    pub accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<AccountTrigger>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub account_event_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<ErrorCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::account::types::{OrderSide, OrderType};

    #[test]
    fn account_actor_roundtrip() {
        for a in [AccountActor::Agent, AccountActor::System, AccountActor::User] {
            assert_eq!(AccountActor::from_str(a.as_str()), Some(a));
        }
    }

    #[test]
    fn operate_account_place_order_deserializes() {
        let json = r#"{
            "action": "place_order",
            "tsCode": "600519.SH",
            "side": "buy",
            "orderType": "limit",
            "limitPrice": "100.00",
            "quantity": 100,
            "reason": "test"
        }"#;
        let req: OperateAccountRequest = serde_json::from_str(json).unwrap();
        match req.action {
            OperateAccountAction::PlaceOrder {
                side, order_type, ..
            } => {
                assert_eq!(side, OrderSide::Buy);
                assert_eq!(order_type, OrderType::Limit);
            }
            _ => panic!("expected place_order"),
        }
    }

    #[test]
    fn update_watchlist_add_deserializes() {
        let json = r#"{
            "action": "add",
            "tsCode": "600519.SH",
            "note": "long-term hold"
        }"#;
        let req: UpdateWatchlistRequest = serde_json::from_str(json).unwrap();
        match req.action {
            UpdateWatchlistAction::Add { note, .. } => {
                assert_eq!(note.as_deref(), Some("long-term hold"));
            }
            _ => panic!("expected add"),
        }
    }

    #[test]
    fn adjust_protection_omit_field_does_not_change() {
        let json = r#"{
            "action": "adjust_protection",
            "positionId": "p1",
            "reason": "no-op"
        }"#;
        let req: OperateAccountRequest = serde_json::from_str(json).unwrap();
        match req.action {
            OperateAccountAction::AdjustProtection {
                stop_loss,
                take_profit,
                time_stop_at,
                ..
            } => {
                assert!(stop_loss.is_none(), "omitted field should be None (no change)");
                assert!(take_profit.is_none());
                assert!(time_stop_at.is_none());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn adjust_protection_explicit_value_sets_field() {
        let json = r#"{
            "action": "adjust_protection",
            "positionId": "p1",
            "stopLoss": "50.00",
            "reason": "set stop"
        }"#;
        let req: OperateAccountRequest = serde_json::from_str(json).unwrap();
        match req.action {
            OperateAccountAction::AdjustProtection { stop_loss, .. } => {
                // Some(Some(price))
                assert!(matches!(stop_loss, Some(Some(_))));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn adjust_protection_distinguishes_clear_from_omit() {
        // 显式 null 表示清除
        let json_clear = r#"{
            "action": "adjust_protection",
            "positionId": "p1",
            "stopLoss": null,
            "reason": "clear stop"
        }"#;
        let req: OperateAccountRequest = serde_json::from_str(json_clear).unwrap();
        match req.action {
            OperateAccountAction::AdjustProtection { stop_loss, .. } => {
                // Some(None) = 清除
                assert!(stop_loss.is_some() && stop_loss.unwrap().is_none());
            }
            _ => panic!(),
        }
    }
}
