//! AccountTrigger / TriggerKey 稳定生成 / 评估结果。
//!
//! Spec: docs/design/account-module.md §2 (触发事件模型)

use crate::domain::shared::{Freshness, OccurredAt, Price, TradeDate, TsCode, WarningCode};
use serde::{Deserialize, Serialize};
use specta::Type;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
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
            Self::StopLoss => "stop_loss",
            Self::TakeProfit => "take_profit",
            Self::TimeStop => "time_stop",
            Self::OrderFilled => "order_filled",
            Self::OrderRejected => "order_rejected",
            Self::OrderExpired => "order_expired",
            Self::Invalidated => "invalidated",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "stop_loss" => Self::StopLoss,
            "take_profit" => Self::TakeProfit,
            "time_stop" => Self::TimeStop,
            "order_filled" => Self::OrderFilled,
            "order_rejected" => Self::OrderRejected,
            "order_expired" => Self::OrderExpired,
            "invalidated" => Self::Invalidated,
            _ => return None,
        })
    }
}

/// Spec: account-module.md §2 AccountTrigger。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AccountTrigger {
    pub trigger_id: String,
    pub trigger_type: AccountTriggerType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts_code: Option<TsCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
    /// 触发阈值（字符串编码），可表示 Price / OccurredAt / signal label。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_freshness: Option<Freshness>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<WarningCode>,
    pub event_id: String,
    pub handled: bool,
    pub occurred_at: OccurredAt,
}

/// 触发评估结果。
///
/// Spec: account-module.md §2 AccountTriggerResult / §5 触发评估。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AccountTriggerResult {
    pub triggers: Vec<AccountTrigger>,
    pub account_event_ids: Vec<String>,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<WarningCode>,
}

/// 触发稳定键 — 用于幂等生成 trigger_id。
///
/// Spec: account-module.md §2 触发事件模型 — `triggerId` 必须按稳定字段确定性生成。
pub enum TriggerKey<'a> {
    /// 价格型持仓保护：`triggerType + positionId + tsCode + protectionRevision + threshold + tradeDate`。
    PriceProtection {
        trigger_type: AccountTriggerType,
        position_id: &'a str,
        ts_code: &'a TsCode,
        protection_revision: u32,
        /// Decimal as string（保证序列化一致）。
        threshold: String,
        trade_date: TradeDate,
    },
    /// 时间止损：`triggerType + positionId + tsCode + protectionRevision + timeStopAt`。
    TimeStop {
        position_id: &'a str,
        ts_code: &'a TsCode,
        protection_revision: u32,
        time_stop_at: OccurredAt,
    },
    /// 失效信号：`triggerType + positionId + tsCode + protectionRevision + signal`。
    Invalidated {
        position_id: &'a str,
        ts_code: &'a TsCode,
        protection_revision: u32,
        signal: &'a str,
    },
    /// 订单终态：`triggerType + orderId + tsCode + 对应终态 AccountEvent.eventId`。
    OrderTerminal {
        trigger_type: AccountTriggerType,
        order_id: &'a str,
        ts_code: &'a TsCode,
        event_id: &'a str,
    },
}

impl<'a> TriggerKey<'a> {
    /// 生成稳定 hash trigger_id。
    pub fn stable_id(&self) -> String {
        let raw = self.canonical_string();
        let digest = Sha256::digest(raw.as_bytes());
        format!("trg_{:x}", digest)
    }

    /// 规范字符串编码（用于哈希、log、可观测）。
    pub fn canonical_string(&self) -> String {
        match self {
            Self::PriceProtection {
                trigger_type,
                position_id,
                ts_code,
                protection_revision,
                threshold,
                trade_date,
            } => format!(
                "{}|{}|{}|{}|{}|{}",
                trigger_type.as_str(),
                position_id,
                ts_code.as_str(),
                protection_revision,
                threshold,
                trade_date.format()
            ),
            Self::TimeStop {
                position_id,
                ts_code,
                protection_revision,
                time_stop_at,
            } => format!(
                "time_stop|{}|{}|{}|{}",
                position_id,
                ts_code.as_str(),
                protection_revision,
                time_stop_at.to_rfc3339()
            ),
            Self::Invalidated {
                position_id,
                ts_code,
                protection_revision,
                signal,
            } => format!(
                "invalidated|{}|{}|{}|{}",
                position_id,
                ts_code.as_str(),
                protection_revision,
                signal
            ),
            Self::OrderTerminal {
                trigger_type,
                order_id,
                ts_code,
                event_id,
            } => format!(
                "{}|{}|{}|{}",
                trigger_type.as_str(),
                order_id,
                ts_code.as_str(),
                event_id
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    fn ts() -> TsCode {
        TsCode::parse("600519.SH").unwrap()
    }

    #[test]
    fn price_protection_id_is_deterministic() {
        let td = TradeDate::parse("20260526").unwrap();
        let k1 = TriggerKey::PriceProtection {
            trigger_type: AccountTriggerType::StopLoss,
            position_id: "pos_1",
            ts_code: &ts(),
            protection_revision: 3,
            threshold: Decimal::new(15000, 2).to_string(),
            trade_date: td,
        };
        let k2 = TriggerKey::PriceProtection {
            trigger_type: AccountTriggerType::StopLoss,
            position_id: "pos_1",
            ts_code: &ts(),
            protection_revision: 3,
            threshold: Decimal::new(15000, 2).to_string(),
            trade_date: td,
        };
        assert_eq!(k1.stable_id(), k2.stable_id());
    }

    #[test]
    fn different_revision_gives_different_id() {
        let td = TradeDate::parse("20260526").unwrap();
        let k1 = TriggerKey::PriceProtection {
            trigger_type: AccountTriggerType::StopLoss,
            position_id: "pos_1",
            ts_code: &ts(),
            protection_revision: 3,
            threshold: "150.00".into(),
            trade_date: td,
        };
        let k2 = TriggerKey::PriceProtection {
            trigger_type: AccountTriggerType::StopLoss,
            position_id: "pos_1",
            ts_code: &ts(),
            protection_revision: 4,
            threshold: "150.00".into(),
            trade_date: td,
        };
        assert_ne!(k1.stable_id(), k2.stable_id());
    }

    #[test]
    fn order_terminal_id_uses_event_id() {
        let k1 = TriggerKey::OrderTerminal {
            trigger_type: AccountTriggerType::OrderFilled,
            order_id: "ord_1",
            ts_code: &ts(),
            event_id: "evt_a",
        };
        let k2 = TriggerKey::OrderTerminal {
            trigger_type: AccountTriggerType::OrderFilled,
            order_id: "ord_1",
            ts_code: &ts(),
            event_id: "evt_b",
        };
        assert_ne!(k1.stable_id(), k2.stable_id());
    }

    #[test]
    fn invalidated_id_uses_signal() {
        let k1 = TriggerKey::Invalidated {
            position_id: "p1",
            ts_code: &ts(),
            protection_revision: 1,
            signal: "earnings_recovery_failed",
        };
        let k2 = TriggerKey::Invalidated {
            position_id: "p1",
            ts_code: &ts(),
            protection_revision: 1,
            signal: "earnings_recovery_failed",
        };
        let k3 = TriggerKey::Invalidated {
            position_id: "p1",
            ts_code: &ts(),
            protection_revision: 1,
            signal: "other_signal",
        };
        assert_eq!(k1.stable_id(), k2.stable_id());
        assert_ne!(k1.stable_id(), k3.stable_id());
    }

    #[test]
    fn trigger_type_roundtrip() {
        for t in [
            AccountTriggerType::StopLoss,
            AccountTriggerType::TakeProfit,
            AccountTriggerType::TimeStop,
            AccountTriggerType::OrderFilled,
            AccountTriggerType::OrderRejected,
            AccountTriggerType::OrderExpired,
            AccountTriggerType::Invalidated,
        ] {
            assert_eq!(AccountTriggerType::from_str(t.as_str()), Some(t));
        }
    }
}
