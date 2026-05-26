//! `Order` / `OrderStatus` / `TradeFill` / `PositionLot` —— spec
//! `account-module.md §2`。
//!
//! 当前阶段以 spec-as-source 类型契约形态落库，写路径（place_order / cancel_order /
//! 挂单评估）尚未接入 AccountService —— 详见 spec §4 / §5 后续重构。引入这层
//! 是为了让 canonical 写入口（`pipeline::account::canonical::dispatch`）能在
//! 类型层有 Order/Fill/Lot 锚点，避免新代码继续围绕"position 一肩挑"模型展开。

use serde::{Deserialize, Serialize};

use crate::domain::shared::{OccurredAt, Shares, TsCode, Yuan};

use super::events::AccountActor;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    pub fn as_str(self) -> &'static str {
        match self {
            OrderSide::Buy => "buy",
            OrderSide::Sell => "sell",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    Market,
    Limit,
}

impl OrderType {
    pub fn as_str(self) -> &'static str {
        match self {
            OrderType::Market => "market",
            OrderType::Limit => "limit",
        }
    }
}

/// Spec §2 `OrderStatus` —— 完整六态状态机。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    pub fn as_str(self) -> &'static str {
        match self {
            OrderStatus::Pending => "pending",
            OrderStatus::PartiallyFilled => "partially_filled",
            OrderStatus::Filled => "filled",
            OrderStatus::Cancelled => "cancelled",
            OrderStatus::Rejected => "rejected",
            OrderStatus::Expired => "expired",
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::Rejected | OrderStatus::Expired
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderIntent {
    OpenPosition,
    ScaleIn,
    ScaleOut,
    ClosePosition,
    DirectOrder,
}

impl OrderIntent {
    pub fn as_str(self) -> &'static str {
        match self {
            OrderIntent::OpenPosition => "open_position",
            OrderIntent::ScaleIn => "scale_in",
            OrderIntent::ScaleOut => "scale_out",
            OrderIntent::ClosePosition => "close_position",
            OrderIntent::DirectOrder => "direct_order",
        }
    }
}

/// Spec §2 `Order` —— canonical 委托模型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Order {
    pub order_id: String,
    pub ts_code: TsCode,
    pub side: OrderSide,
    pub order_type: OrderType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_price: Option<Yuan>,
    pub quantity: Shares,
    pub filled_quantity: Shares,
    pub status: OrderStatus,
    pub intent: OrderIntent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub actor: AccountActor,
    pub created_at: OccurredAt,
    pub updated_at: OccurredAt,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<OccurredAt>,
}

/// Spec §2 `TradeFill`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeFill {
    pub fill_id: String,
    pub order_id: String,
    pub position_id: String,
    pub ts_code: TsCode,
    pub side: OrderSide,
    pub price: Yuan,
    pub quantity: Shares,
    pub commission: Yuan,
    pub stamp_tax: Yuan,
    pub occurred_at: OccurredAt,
}

/// Spec §2 `PositionLot` —— T+1 / 可卖数量 / 冻结的最小重建单元。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionLot {
    pub lot_id: String,
    pub position_id: String,
    pub ts_code: TsCode,
    pub source_fill_id: String,
    /// 买入成交交易日（YYYYMMDD）
    pub trade_date: String,
    pub quantity: Shares,
    pub remaining_quantity: Shares,
    pub frozen_quantity: Shares,
    /// 最早可卖交易日；股票 / 场内基金为买入下一交易日（YYYYMMDD）
    pub sellable_from: String,
    pub created_at: OccurredAt,
}
