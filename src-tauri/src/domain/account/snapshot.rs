//! Account derived snapshot —— spec `account-module.md §2 账户快照`。

use super::position::Position;
use crate::domain::shared::{Freshness, OccurredAt, WarningCode, Yuan};
use serde::{Deserialize, Serialize};

fn yuan_zero() -> Yuan {
    Yuan::ZERO
}

/// spec `account-module.md §2 AccountSnapshot` 派生快照。所有字段都是从
/// account_events + positions + Quotes snapshot 现算派生；不持久化为真源。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSnapshot {
    pub initial_cash: Yuan,
    pub cash: Yuan,
    /// spec `availableCash = cash - frozenCash`
    #[serde(default = "yuan_zero")]
    pub available_cash: Yuan,
    /// spec `frozenCash`：未完成 buy order 剩余冻结金额合计
    #[serde(default = "yuan_zero")]
    pub frozen_cash: Yuan,
    pub open_positions: Vec<Position>,
    pub closed_positions: Vec<Position>,
    pub market_value: Yuan,
    pub realized_pnl: Yuan,
    pub unrealized_pnl: Yuan,
    pub total_pnl: Yuan,
    pub total_assets: Yuan,
    pub captured_at: OccurredAt,
    /// spec §2「AccountSnapshot.pricedPositionCount / unpricedPositionCount」
    /// 表达估值覆盖范围；部分仓位缺行情时配合 warnings.data_partial 一起返回。
    #[serde(default)]
    pub priced_position_count: usize,
    #[serde(default)]
    pub unpriced_position_count: usize,
    /// spec `openPositionCount`：派生自 open_positions.len()
    #[serde(default)]
    pub open_position_count: usize,
    /// spec `pendingOrderCount`：未完成 order 数量；由 valuation 注入
    #[serde(default)]
    pub pending_order_count: usize,
    /// 整体估值新鲜度（取所有 open position 行情中最 worst 的状态）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valuation_freshness: Option<Freshness>,
    /// spec §2「warnings 例如 quote_missing / quote_stale / data_partial」
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<WarningCode>,
}
