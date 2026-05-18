//! Account 主指标——验证 agent 是否有效的 ground truth。
//!
//! 见 docs/design/agent-redesign.md § 5.5 主指标列表。
//!
//! 主指标 = 模拟账户的 PnL 维度（绝对值，不需要 baseline）：
//! - cumulative_return = total_pnl / INITIAL_CAPITAL
//! - max_drawdown = 累计资产曲线的最大回撤
//! - win_rate = closed positions 中盈利占比
//! - avg_holding_days = closed positions 的平均持仓天数
//! - thesis_hit_rate = 状态走到 validated 的 thesis / 总 closed thesis
//! - invalidation_hit_avg_loss = 触发 invalidation 后的平均亏损（绝对值）
//!
//! 实现：从已存在的 PositionEvent + simulated_positions + theses 派生。
//! 不存中间表——每次现算（小数据量足够；后续 N>1000 再考虑预聚合）。

use crate::domain::account::{Position, PositionStatus};
use crate::infrastructure::account::valuation::INITIAL_CASH;
use crate::infrastructure::db::{migrate, open_database};
use serde::Serialize;
use tauri::AppHandle;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountMetrics {
    pub cumulative_return: f64,                // 0.05 = 5%
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub total_assets: f64,
    pub win_rate: Option<f64>,                 // 样本不足返 None
    pub total_closed: u32,
    pub winning_closed: u32,
    pub avg_holding_days: Option<f64>,
    pub thesis_hit_rate: Option<f64>,          // validated / total closed thesis
    pub thesis_invalidated_count: u32,
}

pub fn compute(
    app: &AppHandle,
    positions: &[Position],
    snapshot_total_assets: f64,
    snapshot_realized_pnl: f64,
    snapshot_unrealized_pnl: f64,
) -> Result<AccountMetrics, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;

    let cumulative_return = if INITIAL_CASH > 0.0 {
        (snapshot_total_assets - INITIAL_CASH) / INITIAL_CASH
    } else {
        0.0
    };

    // Win rate + 持仓天数（基于内存 positions 直接算，避免重复 IO）
    let mut total_closed: u32 = 0;
    let mut winning_closed: u32 = 0;
    let mut holding_days_sum: f64 = 0.0;
    for p in positions {
        if let PositionStatus::Closed {
            exit_price, exit_at, ..
        } = &p.status
        {
            total_closed += 1;
            let pnl_per_share = exit_price.value() - p.avg_entry_price.value();
            if pnl_per_share > 0.0 {
                winning_closed += 1;
            }
            let entered_ms = p.entered_at.value();
            let exit_ms = exit_at.value();
            let days = (exit_ms - entered_ms) as f64 / (1000.0 * 86400.0);
            if days >= 0.0 {
                holding_days_sum += days;
            }
        }
    }
    let win_rate = if total_closed > 0 {
        Some(winning_closed as f64 / total_closed as f64)
    } else {
        None
    };
    let avg_holding_days = if total_closed > 0 {
        Some(holding_days_sum / total_closed as f64)
    } else {
        None
    };

    // Thesis hit rate
    let (total_terminal, validated_count): (u32, u32) = conn
        .query_row(
            "select
                sum(case when state in ('validated','drifted','invalidated','abandoned') then 1 else 0 end),
                sum(case when state = 'validated' then 1 else 0 end)
             from theses",
            [],
            |row| {
                let total: Option<u32> = row.get(0).ok();
                let val: Option<u32> = row.get(1).ok();
                Ok((total.unwrap_or(0), val.unwrap_or(0)))
            },
        )
        .unwrap_or((0, 0));
    let thesis_hit_rate = if total_terminal > 0 {
        Some(validated_count as f64 / total_terminal as f64)
    } else {
        None
    };

    let thesis_invalidated_count: u32 = conn
        .query_row(
            "select count(*) from theses where state = 'invalidated'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    Ok(AccountMetrics {
        cumulative_return,
        realized_pnl: snapshot_realized_pnl,
        unrealized_pnl: snapshot_unrealized_pnl,
        total_assets: snapshot_total_assets,
        win_rate,
        total_closed,
        winning_closed,
        avg_holding_days,
        thesis_hit_rate,
        thesis_invalidated_count,
    })
}
