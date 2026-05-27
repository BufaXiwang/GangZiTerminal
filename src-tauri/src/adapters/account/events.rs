//! Account BC 对外事件常量 + envelope helper。
//!
//! Spec: docs/design/account-module.md §3 数据流 / shared-types.md §6
//!
//! 事件名：
//! - `account-updated`   — 账户读模型变化（spec §3 写入流）
//! - `account-triggered` — 新 trigger 生成（spec §3 触发评估流）

use crate::domain::account::triggers::AccountTriggerType;
use crate::domain::shared::{AppEventEnvelope, Freshness, OccurredAt, TsCode, WarningCode};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

pub const ACCOUNT_UPDATED_EVENT: &str = "account-updated";
pub const ACCOUNT_TRIGGERED_EVENT: &str = "account-triggered";

/// Spec: shared-types.md §6 — AccountUpdatedPayload。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AccountUpdatedPayload {
    pub account_event_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub affected_order_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub affected_position_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub affected_ts_codes: Vec<TsCode>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub affected_watchlist_ts_codes: Vec<TsCode>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub trigger_ids: Vec<String>,
    pub snapshot_captured_at: OccurredAt,
}

/// Spec: shared-types.md §6 — AccountTriggeredPayload。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AccountTriggeredPayload {
    pub trigger_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts_code: Option<TsCode>,
    pub trigger_type: AccountTriggerType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_freshness: Option<Freshness>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<WarningCode>,
}

pub fn wrap_account_updated(
    inner: crate::pipeline::account::service::AccountUpdatedPayloadInner,
    correlation_id: Option<String>,
) -> AppEventEnvelope<AccountUpdatedPayload> {
    AppEventEnvelope {
        event_id: Uuid::new_v4().to_string(),
        event_type: ACCOUNT_UPDATED_EVENT.into(),
        occurred_at: Utc::now(),
        correlation_id,
        causation_id: None,
        payload: AccountUpdatedPayload {
            account_event_ids: inner.account_event_ids,
            affected_order_ids: inner.affected_order_ids,
            affected_position_ids: inner.affected_position_ids,
            affected_ts_codes: inner.affected_ts_codes,
            affected_watchlist_ts_codes: inner.affected_watchlist_ts_codes,
            trigger_ids: inner.trigger_ids,
            snapshot_captured_at: inner.snapshot_captured_at,
        },
    }
}

pub fn wrap_account_triggered(
    inner: crate::pipeline::account::service::AccountTriggeredPayloadInner,
    correlation_id: Option<String>,
) -> AppEventEnvelope<AccountTriggeredPayload> {
    AppEventEnvelope {
        event_id: Uuid::new_v4().to_string(),
        event_type: ACCOUNT_TRIGGERED_EVENT.into(),
        occurred_at: Utc::now(),
        correlation_id,
        causation_id: None,
        payload: AccountTriggeredPayload {
            trigger_id: inner.trigger.trigger_id,
            position_id: inner.trigger.position_id,
            order_id: inner.trigger.order_id,
            ts_code: inner.trigger.ts_code,
            trigger_type: inner.trigger.trigger_type,
            quote_freshness: inner.trigger.quote_freshness,
            warnings: inner.trigger.warnings,
        },
    }
}
