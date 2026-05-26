//! `AccountTrigger` 模型 —— spec `account-module.md §2`。
//!
//! Trigger 是 Account 内部条件命中（保护条件 / 订单终态 / 失效信号）
//! 的跨模块通知事件。Account 只生成 trigger 并 emit；下游决策方
//! （Agent Runtime）消费后调用 `mark_trigger_handled`。

use serde::{Deserialize, Serialize};

use super::position::CloseReason;
use crate::domain::shared::{Freshness, WarningCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountTriggerType {
    StopLoss,
    TakeProfit,
    TimeStop,
    OrderFilled,
    OrderRejected,
    OrderExpired,
    Invalidated,
}

impl AccountTriggerType {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountTriggerType::StopLoss => "stop_loss",
            AccountTriggerType::TakeProfit => "take_profit",
            AccountTriggerType::TimeStop => "time_stop",
            AccountTriggerType::OrderFilled => "order_filled",
            AccountTriggerType::OrderRejected => "order_rejected",
            AccountTriggerType::OrderExpired => "order_expired",
            AccountTriggerType::Invalidated => "invalidated",
        }
    }

    /// 从 close_reason 派生 trigger 类型。spec §3「保护条件触发」。
    pub fn from_close_reason(reason: CloseReason) -> Option<Self> {
        match reason {
            CloseReason::StopLoss => Some(AccountTriggerType::StopLoss),
            CloseReason::TakeProfit => Some(AccountTriggerType::TakeProfit),
            CloseReason::TimeStop => Some(AccountTriggerType::TimeStop),
            CloseReason::Invalidated => Some(AccountTriggerType::Invalidated),
            CloseReason::Manual => None,
        }
    }
}

/// spec §2 `AccountTrigger.threshold`：价格型阈值 / 时间止损时间 / 自由 signal 字符串。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerThreshold {
    Price { value: f64 },
    Time { at: String },
    Signal { value: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountTrigger {
    pub trigger_id: String,
    pub trigger_type: AccountTriggerType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<f64>,
    /// spec §2：价格 / 时间 / signal 阈值；保护条件 / 时间止损 / 失效信号必填。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<TriggerThreshold>,
    /// spec §2：价格型 trigger 必填，承载所用 quote freshness。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_freshness: Option<Freshness>,
    /// spec §2：触发警告（例如使用 stale quote 时含 `quote_stale`）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<WarningCode>,
    pub event_id: String,
    pub handled: bool,
    pub occurred_at: String,
}

/// spec §2 稳定 triggerId 规则——按 trigger 类型确定性派生。
///
/// 这里只暴露 3 个 helper，调用方提供必要语义字段；hash 用 SHA-256 保证跨进程稳定。
///
/// - `price_protection`: `type + positionId + tsCode + protectionRevision + threshold + tradeDate`
/// - `time_stop`:        `type + positionId + tsCode + protectionRevision + timeStopAt`
/// - `invalidation`:     `type + positionId + tsCode + protectionRevision + signal`
/// - `order_terminal`:   `type + orderId + tsCode + 对应终态 AccountEvent.eventId`
fn hash_id(prefix: &str, parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut s = String::new();
    s.push_str(prefix);
    for p in parts {
        s.push('|');
        s.push_str(p);
    }
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let digest = h.finalize();
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest.iter() {
        let _ = write!(hex, "{b:02x}");
    }
    format!("trig_{}", &hex[..32])
}

pub fn derive_price_protection_trigger_id(
    trigger_type: AccountTriggerType,
    position_id: &str,
    ts_code: &str,
    protection_revision: u32,
    threshold: f64,
    trade_date: &str,
) -> String {
    debug_assert!(matches!(
        trigger_type,
        AccountTriggerType::StopLoss | AccountTriggerType::TakeProfit
    ));
    let rev = protection_revision.to_string();
    let th = format!("{threshold:.6}");
    hash_id(
        trigger_type.as_str(),
        &[position_id, ts_code, &rev, &th, trade_date],
    )
}

pub fn derive_time_stop_trigger_id(
    position_id: &str,
    ts_code: &str,
    protection_revision: u32,
    time_stop_at: &str,
) -> String {
    let rev = protection_revision.to_string();
    hash_id(
        AccountTriggerType::TimeStop.as_str(),
        &[position_id, ts_code, &rev, time_stop_at],
    )
}

pub fn derive_invalidation_trigger_id(
    position_id: &str,
    ts_code: &str,
    protection_revision: u32,
    signal: &str,
) -> String {
    let rev = protection_revision.to_string();
    hash_id(
        AccountTriggerType::Invalidated.as_str(),
        &[position_id, ts_code, &rev, signal],
    )
}

pub fn derive_order_terminal_trigger_id(
    trigger_type: AccountTriggerType,
    order_id: &str,
    ts_code: &str,
    event_id: &str,
) -> String {
    debug_assert!(matches!(
        trigger_type,
        AccountTriggerType::OrderFilled
            | AccountTriggerType::OrderRejected
            | AccountTriggerType::OrderExpired
    ));
    hash_id(trigger_type.as_str(), &[order_id, ts_code, event_id])
}

/// 兼容旧 close_event 派生路径：当前 AccountService 仍按 close event 触发保护命中，
/// 没有 `protectionRevision`，使用 `revision = 1` 占位 + close `event_id` 替代 `threshold`。
///
/// 同一 close event 不重复生成 trigger 由 `event_id` 唯一性保证。
pub fn derive_trigger_id_from_close(
    trigger_type: AccountTriggerType,
    position_id: &str,
    ts_code: &str,
    event_id: &str,
) -> String {
    hash_id(trigger_type.as_str(), &[position_id, ts_code, "1", event_id])
}
