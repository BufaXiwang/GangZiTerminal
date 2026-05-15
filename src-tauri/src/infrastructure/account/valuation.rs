//! 账户估值——从 positions + events + MARKET_SNAPSHOT 派生 AccountSnapshot。
//!
//! 三块派生：
//!
//! 1. **cash** = initial_cash + Σ event_cash_delta（含费）
//!    `domain::account::cash::reduce_events_to_cash_delta` 算
//!
//! 2. **market_value** = Σ open positions × 当前价（来自 MARKET_SNAPSHOT）
//!    缺价时 fallback 到 avg_entry_price
//!
//! 3. **realized_pnl** = closed positions 一个 cycle 的 cash 净流入（含费）
//!    **unrealized_pnl** = (current_price - avg_entry) × current_shares（不含将来的费，毛浮盈）
//!    **total_pnl** = realized + unrealized
//!
//! 依赖：本模块**读** MARKET_SNAPSHOT（account → quotes，spec § 1.3 允许）。

use crate::domain::account::cash::reduce_events_to_cash_delta;
use crate::domain::account::types::{AccountSnapshot, Position, PositionEvent, PositionStatus};
use crate::domain::shared::{OccurredAt, Yuan};
use crate::infrastructure::quotes::snapshot::market_snapshot;
use std::collections::HashMap;

/// 模拟账户初始现金——20000 元。硬编码，第一版不配置化。
pub const INITIAL_CASH: f64 = 20000.0;

/// 从 positions + 所有相关 events 派生当前 AccountSnapshot。
///
/// **events 应该已经按 occurred_at 升序排好**（repository::list_events_batch 已经做了）。
pub fn compute_snapshot(positions: &[Position], events: &[PositionEvent]) -> AccountSnapshot {
    let initial_cash = Yuan::from_unchecked(INITIAL_CASH);

    // ----- cash -----
    let cash_delta = reduce_events_to_cash_delta(events);
    let cash = Yuan::from_unchecked(initial_cash.value() + cash_delta.value());

    // ----- market_value + unrealized -----
    let mut market_value = 0.0;
    let mut unrealized_pnl = 0.0;
    for p in positions.iter().filter(|p| p.status.is_open()) {
        let ts_code = p.code.to_ts_code();
        let current_price = market_snapshot::get(&ts_code)
            .and_then(|q| q.price)
            .map(|y| y.value())
            .unwrap_or(p.avg_entry_price.value()); // 拿不到价就用均价兜底（unrealized 显示 0）
        let value = current_price * p.current_shares.value() as f64;
        market_value += value;
        let cost = p.avg_entry_price.value() * p.current_shares.value() as f64;
        unrealized_pnl += value - cost;
    }

    // ----- realized_pnl: 已平仓 positions 的事件链净 cash 流 -----
    let realized_pnl = compute_realized_pnl(positions, events);

    let total_pnl = realized_pnl + unrealized_pnl;
    let total_assets = cash.value() + market_value;

    let (open_positions, closed_positions): (Vec<_>, Vec<_>) =
        positions.iter().cloned().partition(|p| p.status.is_open());

    AccountSnapshot {
        initial_cash,
        cash,
        open_positions,
        closed_positions,
        market_value: Yuan::from_unchecked(market_value),
        realized_pnl: Yuan::from_unchecked(realized_pnl),
        unrealized_pnl: Yuan::from_unchecked(unrealized_pnl),
        total_pnl: Yuan::from_unchecked(total_pnl),
        total_assets: Yuan::from_unchecked(total_assets),
        captured_at: OccurredAt::now(),
    }
}

/// 已实现盈亏 = 每个 closed position 完整 cycle (open → close) 的 cash 净流入。
///
/// 对单个 closed position 的事件链 reduce 得到的数即 PnL（含费扣除）。
fn compute_realized_pnl(positions: &[Position], events: &[PositionEvent]) -> f64 {
    let mut by_pos: HashMap<&str, Vec<&PositionEvent>> = HashMap::new();
    for e in events {
        by_pos.entry(e.position_id.as_str()).or_default().push(e);
    }
    let mut realized = 0.0;
    for p in positions
        .iter()
        .filter(|p| matches!(p.status, PositionStatus::Closed { .. }))
    {
        if let Some(es) = by_pos.get(p.id.as_str()) {
            let owned: Vec<PositionEvent> = es.iter().map(|e| (*e).clone()).collect();
            let delta = reduce_events_to_cash_delta(&owned);
            realized += delta.value();
        }
    }
    realized
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::account::types::{
        CloseReason, EventSource, PositionEventKind, PositionId, PositionStatus,
    };
    use crate::domain::shared::{Shares, StockCode};

    fn open_event(pos_id: &str, entry: f64, shares: i64, commission: f64) -> PositionEvent {
        PositionEvent {
            id: format!("e-open-{pos_id}"),
            position_id: PositionId::from_string(pos_id.into()),
            kind: PositionEventKind::Opened {
                entry_price: Yuan::new(entry).unwrap(),
                shares: Shares::new(shares).unwrap(),
                commission: Yuan::new(commission).unwrap(),
            },
            occurred_at: OccurredAt::new(1),
            source: EventSource::Manual,
            agent_note_md: String::new(),
        }
    }

    fn closed_event(
        pos_id: &str,
        exit: f64,
        shares: i64,
        commission: f64,
        stamp_tax: f64,
    ) -> PositionEvent {
        PositionEvent {
            id: format!("e-close-{pos_id}"),
            position_id: PositionId::from_string(pos_id.into()),
            kind: PositionEventKind::Closed {
                exit_price: Yuan::new(exit).unwrap(),
                shares: Shares::new(shares).unwrap(),
                reason: CloseReason::Manual,
                commission: Yuan::new(commission).unwrap(),
                stamp_tax: Yuan::new(stamp_tax).unwrap(),
            },
            occurred_at: OccurredAt::new(2),
            source: EventSource::Manual,
            agent_note_md: String::new(),
        }
    }

    fn open_position(id: &str, code: &str, avg: f64, shares: i64) -> Position {
        Position {
            id: PositionId::from_string(id.into()),
            code: StockCode::new(code).unwrap(),
            name: "test".into(),
            avg_entry_price: Yuan::new(avg).unwrap(),
            current_shares: Shares::new(shares).unwrap(),
            status: PositionStatus::Open,
            stop_loss: None,
            take_profit: None,
            time_stop_at: None,
            thesis: String::new(),
            source_analysis_id: String::new(),
            entered_at: OccurredAt::new(1),
        }
    }

    fn closed_position(id: &str, code: &str, avg: f64, shares: i64, exit: f64) -> Position {
        Position {
            id: PositionId::from_string(id.into()),
            code: StockCode::new(code).unwrap(),
            name: "test".into(),
            avg_entry_price: Yuan::new(avg).unwrap(),
            current_shares: Shares::new(shares).unwrap(),
            status: PositionStatus::Closed {
                exit_price: Yuan::new(exit).unwrap(),
                exit_at: OccurredAt::new(2),
                reason: CloseReason::Manual,
            },
            stop_loss: None,
            take_profit: None,
            time_stop_at: None,
            thesis: String::new(),
            source_analysis_id: String::new(),
            entered_at: OccurredAt::new(1),
        }
    }

    #[test]
    fn empty_account_snapshot() {
        let snap = compute_snapshot(&[], &[]);
        assert_eq!(snap.cash.value(), INITIAL_CASH);
        assert_eq!(snap.market_value.value(), 0.0);
        assert_eq!(snap.total_pnl.value(), 0.0);
        assert_eq!(snap.total_assets.value(), INITIAL_CASH);
    }

    #[test]
    fn realized_pnl_closed_cycle() {
        // 开 100 股 @ 10 → 平 100 股 @ 12
        // 开 cost: -1000 - 5 = -1005
        // 平 proceeds: 1200 - 5 - 1.2 = 1193.8
        // realized: 188.8
        let p = closed_position("p1", "600519", 10.0, 100, 12.0);
        let events = vec![
            open_event("p1", 10.0, 100, 5.0),
            closed_event("p1", 12.0, 100, 5.0, 1.2),
        ];
        let snap = compute_snapshot(&[p], &events);
        assert!((snap.realized_pnl.value() - 188.8).abs() < 1e-6);
        assert!((snap.cash.value() - (INITIAL_CASH + 188.8)).abs() < 1e-6);
        assert_eq!(snap.unrealized_pnl.value(), 0.0); // 无 open positions
        assert!((snap.total_pnl.value() - 188.8).abs() < 1e-6);
    }

    #[test]
    fn unrealized_when_no_market_price_uses_avg() {
        // open 100 股 @ 10，无 MARKET_SNAPSHOT 数据 → unrealized = 0
        let p = open_position("p1", "600519", 10.0, 100);
        let events = vec![open_event("p1", 10.0, 100, 5.0)];
        let snap = compute_snapshot(&[p], &events);
        // market_value 用 avg 兜底 = 10×100 = 1000
        assert_eq!(snap.market_value.value(), 1000.0);
        assert_eq!(snap.unrealized_pnl.value(), 0.0);
        // cash = INITIAL - 1005
        assert!((snap.cash.value() - (INITIAL_CASH - 1005.0)).abs() < 1e-6);
    }

    #[test]
    fn total_assets_equals_cash_plus_market_value() {
        // 多仓位混合
        let p1 = closed_position("p1", "600519", 10.0, 100, 12.0);
        let p2 = open_position("p2", "000001", 20.0, 200);
        let events = vec![
            open_event("p1", 10.0, 100, 5.0),
            closed_event("p1", 12.0, 100, 5.0, 1.2),
            open_event("p2", 20.0, 200, 5.0),
        ];
        let snap = compute_snapshot(&[p1, p2], &events);
        // p1 realized 188.8
        // p2 cash impact: -20×200 - 5 = -4005
        // total cash delta: 188.8 - 4005 = -3816.2
        let expected_cash = INITIAL_CASH - 3816.2;
        assert!((snap.cash.value() - expected_cash).abs() < 1e-6);
        // p2 market_value (无 SNAPSHOT 用 avg 兜底): 20×200 = 4000
        assert!((snap.market_value.value() - 4000.0).abs() < 1e-6);
        // total_assets = cash + market_value
        assert!((snap.total_assets.value() - (expected_cash + 4000.0)).abs() < 1e-6);
    }

    #[test]
    fn partitions_open_vs_closed() {
        let p1 = open_position("p1", "600519", 10.0, 100);
        let p2 = closed_position("p2", "000001", 20.0, 200, 22.0);
        let events = vec![
            open_event("p1", 10.0, 100, 5.0),
            open_event("p2", 20.0, 200, 5.0),
            closed_event("p2", 22.0, 200, 5.0, 4.4),
        ];
        let snap = compute_snapshot(&[p1, p2], &events);
        assert_eq!(snap.open_positions.len(), 1);
        assert_eq!(snap.closed_positions.len(), 1);
    }
}
