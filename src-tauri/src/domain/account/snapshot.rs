//! Account derived snapshot —— spec `account-module.md §2 账户快照`。

use super::position::Position;
use crate::domain::shared::{Freshness, OccurredAt, WarningCode, Yuan};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSnapshot {
    pub initial_cash: Yuan,
    pub cash: Yuan,
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
    /// 整体估值新鲜度（取所有 open position 行情中最 worst 的状态）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valuation_freshness: Option<Freshness>,
    /// spec §2「warnings 例如 quote_missing / quote_stale / data_partial」
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<WarningCode>,
}
