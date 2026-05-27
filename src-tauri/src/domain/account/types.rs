//! Account domain types — Order / TradeFill / Position / PositionLot / Protection / Watchlist / Snapshot。
//!
//! Spec: docs/design/account-module.md §2

use crate::domain::shared::{
    Freshness, Money, OccurredAt, Percent, Price, Shares, TradeDate, TsCode, Volume, WarningCode,
};
use crate::domain::shared::Amount;
use serde::{Deserialize, Serialize};
use specta::Type;

// ----------------------------------------------------------------------------
// Actor 枚举
// ----------------------------------------------------------------------------

/// Trading actor — 当前订单 / 仓位只能由 `agent` 发起。
///
/// Spec: account-module.md §2 actor 命名规则。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum TradingActor {
    Agent,
}

// ----------------------------------------------------------------------------
// Order
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Pending,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
    Expired,
}

impl OrderStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Filled | Self::Cancelled | Self::Rejected | Self::Expired
        )
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Pending | Self::PartiallyFilled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum OrderType {
    Market,
    Limit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum OrderIntent {
    OpenPosition,
    ScaleIn,
    ScaleOut,
    ClosePosition,
    DirectOrder,
}

/// Spec: account-module.md §2 订单模型
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct Order {
    pub order_id: String,
    pub ts_code: TsCode,
    pub side: OrderSide,
    pub order_type: OrderType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_price: Option<Price>,
    pub quantity: Shares,
    pub filled_quantity: Shares,
    pub status: OrderStatus,
    pub intent: OrderIntent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub actor: TradingActor,
    pub created_at: OccurredAt,
    pub updated_at: OccurredAt,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<OccurredAt>,
}

// ----------------------------------------------------------------------------
// TradeFill
// ----------------------------------------------------------------------------

/// Spec: account-module.md §2 成交模型
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TradeFill {
    pub fill_id: String,
    pub order_id: String,
    pub position_id: String,
    pub ts_code: TsCode,
    pub side: OrderSide,
    pub price: Price,
    pub quantity: Shares,
    pub commission: Money,
    pub stamp_tax: Money,
    /// 过户费 — 仅 SH 标的 stock / fund 双向收取，其他市场为 0。
    /// Spec: account-module.md §2 成交模型 / 硬风控模型。
    pub transfer_fee: Money,
    pub occurred_at: OccurredAt,
}

// ----------------------------------------------------------------------------
// PositionLot
// ----------------------------------------------------------------------------

/// Spec: account-module.md §2 持仓批次模型
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PositionLot {
    pub lot_id: String,
    pub position_id: String,
    pub ts_code: TsCode,
    pub source_fill_id: String,
    pub trade_date: TradeDate,
    pub quantity: Shares,
    pub remaining_quantity: Shares,
    pub frozen_quantity: Shares,
    pub sellable_from: TradeDate,
    pub created_at: OccurredAt,
}

// ----------------------------------------------------------------------------
// Position
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum PositionStatus {
    Open,
    Closed,
}

/// Spec: account-module.md §2 仓位模型
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub position_id: String,
    pub ts_code: TsCode,
    pub name: String,
    pub status: PositionStatus,
    pub quantity: Shares,
    pub sellable_quantity: Shares,
    pub avg_cost: Price,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_price: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_value: Option<Money>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_freshness: Option<Freshness>,
    pub realized_pnl: Money,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unrealized_pnl: Option<Money>,
    pub opened_at: OccurredAt,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<OccurredAt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protection: Option<PositionProtection>,
    pub actor: TradingActor,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<WarningCode>,
}

// ----------------------------------------------------------------------------
// PositionProtection
// ----------------------------------------------------------------------------

/// Spec: account-module.md §2 保护条件模型
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PositionProtection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_loss: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub take_profit: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_stop_at: Option<OccurredAt>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub invalidation_signals: Vec<String>,
    pub enabled: bool,
    pub revision: u32,
    pub updated_at: OccurredAt,
}

impl PositionProtection {
    pub fn is_empty(&self) -> bool {
        self.stop_loss.is_none()
            && self.take_profit.is_none()
            && self.time_stop_at.is_none()
            && self.invalidation_signals.is_empty()
    }
}

// ----------------------------------------------------------------------------
// Watchlist
// ----------------------------------------------------------------------------

/// Spec: account-module.md §2 自选模型
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct WatchlistItem {
    pub ts_code: TsCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub added_at: OccurredAt,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 自选行情字段。来自 Quotes snapshot；不存在时 freshness = missing。
///
/// Spec: account-module.md §4 (fetch_account watchlist quote 字段)
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct WatchlistQuoteView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_percent: Option<Percent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<Volume>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<Amount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<Freshness>,
}

/// 自选展示模型：WatchlistItem + 可选 quote。
///
/// Spec: account-module.md §4 fetch_account.include.watchlist
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct WatchlistItemView {
    #[serde(flatten)]
    pub item: WatchlistItem,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<WatchlistQuoteView>,
}

// ----------------------------------------------------------------------------
// AccountSnapshot
// ----------------------------------------------------------------------------

/// Spec: account-module.md §2 账户快照
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AccountSnapshot {
    pub initial_cash: Money,
    pub cash: Money,
    pub available_cash: Money,
    pub frozen_cash: Money,
    pub market_value: Money,
    pub total_assets: Money,
    pub realized_pnl: Money,
    pub unrealized_pnl: Money,
    pub total_pnl: Money,
    pub priced_position_count: u32,
    pub unpriced_position_count: u32,
    pub valuation_freshness: Freshness,
    pub open_position_count: u32,
    pub pending_order_count: u32,
    pub captured_at: OccurredAt,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<WarningCode>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_status_terminal_check() {
        assert!(OrderStatus::Filled.is_terminal());
        assert!(OrderStatus::Cancelled.is_terminal());
        assert!(OrderStatus::Rejected.is_terminal());
        assert!(OrderStatus::Expired.is_terminal());
        assert!(!OrderStatus::Pending.is_terminal());
        assert!(!OrderStatus::PartiallyFilled.is_terminal());
    }

    #[test]
    fn order_status_active_check() {
        assert!(OrderStatus::Pending.is_active());
        assert!(OrderStatus::PartiallyFilled.is_active());
        assert!(!OrderStatus::Filled.is_active());
    }

    #[test]
    fn protection_empty_when_no_fields() {
        let p = PositionProtection {
            stop_loss: None,
            take_profit: None,
            time_stop_at: None,
            invalidation_signals: vec![],
            enabled: true,
            revision: 1,
            updated_at: chrono::Utc::now(),
        };
        assert!(p.is_empty());
    }

    #[test]
    fn protection_not_empty_with_stop_loss() {
        use rust_decimal::Decimal;
        let p = PositionProtection {
            stop_loss: Some(Price(Decimal::new(1000, 2))),
            take_profit: None,
            time_stop_at: None,
            invalidation_signals: vec![],
            enabled: true,
            revision: 1,
            updated_at: chrono::Utc::now(),
        };
        assert!(!p.is_empty());
    }
}
