//! Account derived snapshot.

use super::position::Position;
use crate::domain::shared::{OccurredAt, Yuan};
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
}
