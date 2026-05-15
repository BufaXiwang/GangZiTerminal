//! 模拟账户快照计算——learning.rs 推导学习画像 PnL 时用。
//!
//! 注：风险告警（evaluate_simulation_risk 的 TS 端口）目前在前端 src/lib/simulation.ts，
//! 用于 UI 渲染 RiskAlert 列表；Rust 端不参与这步派生，所以这里只保留账户聚合一项。

use crate::agent_io::SimulatedPosition;
use crate::domain::quotes::StockQuote;

pub struct SimulationAccountSnapshot {
    pub total_pnl: f64,
}

pub fn calculate_simulation_account(
    initial_cash: f64,
    positions: &[SimulatedPosition],
    quotes: &[StockQuote],
) -> SimulationAccountSnapshot {
    let mut market_value: f64 = 0.0;
    let mut invested: f64 = 0.0;
    let mut realized_proceeds: f64 = 0.0;
    let mut realized_cost: f64 = 0.0;
    for p in positions {
        // 用 avg_entry_price + 当前实际持仓股数（current_shares）而不是首次开仓
        // ——加仓后均价会变，原 entry_price/shares 只是首次档案
        let avg = p.avg_entry_price();
        let current = p.current_shares() as f64;
        let cost = avg * current;
        match p.status.as_str() {
            "open" => {
                let price = quotes
                    .iter()
                    .find(|q| q.code.as_str() == p.code)
                    .and_then(|q| q.price)
                    .map(|y| y.value())
                    .unwrap_or(avg);
                market_value += price * current;
                invested += cost;
            }
            "closed" => {
                realized_proceeds += p.exit_price.unwrap_or(avg) * current;
                realized_cost += cost;
            }
            _ => {}
        }
    }
    let total_assets = initial_cash - invested - realized_cost + realized_proceeds + market_value;
    SimulationAccountSnapshot {
        total_pnl: total_assets - initial_cash,
    }
}
