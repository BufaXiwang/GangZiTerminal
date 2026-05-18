//! 现金派生——从事件链 reduce 出 cash delta。
//!
//! 设计：**事件是真源**，cash 永不持久化，每次需要时从事件链 walk 一遍算出来。
//!
//! ```
//! cash = INITIAL_CASH + Σ cash_delta(event)
//! ```
//!
//! 一次性计算多个 position 的 cash 影响时：
//! ```rust
//! let delta = reduce_events_to_cash_delta(&all_events);
//! ```
//!
//! 同样地 realized_pnl 派生 = Σ (closed events).net_proceeds —— 同一份事件链 walk 两次。

use super::types::{PositionEvent, PositionEventKind};
use crate::domain::shared::Yuan;

/// 一组事件对 cash 的净影响（**会扣除手续费 / 印花税**）。
///
/// 正数 = 净流入，负数 = 净流出。
pub fn reduce_events_to_cash_delta(events: &[PositionEvent]) -> Yuan {
    let mut delta = 0.0_f64;
    for e in events {
        delta += single_event_cash_delta(&e.kind);
    }
    Yuan::from_unchecked(delta)
}

/// 单事件的 cash 净影响（含费）。
pub fn single_event_cash_delta(kind: &PositionEventKind) -> f64 {
    match kind {
        PositionEventKind::Opened {
            entry_price,
            shares,
            commission,
        } => -(entry_price.value() * shares.value() as f64) - commission.value(),
        PositionEventKind::ScaledIn {
            delta,
            price,
            commission,
            ..
        } => -(price.value() * delta.value() as f64) - commission.value(),
        PositionEventKind::ScaledOut {
            delta,
            price,
            commission,
            stamp_tax,
        } => (price.value() * delta.value() as f64) - commission.value() - stamp_tax.value(),
        PositionEventKind::Closed {
            exit_price,
            shares,
            commission,
            stamp_tax,
            ..
        } => (exit_price.value() * shares.value() as f64) - commission.value() - stamp_tax.value(),
        PositionEventKind::StopsAdjusted { .. }
        | PositionEventKind::Signal { .. } => 0.0, // 审计/调参事件不动现金
    }
}

/// 计算 realized PnL = Σ (exit - avg_entry) × shares - 费 。
///
/// 这里**需要 avg_entry**——只看事件链算不出，因为 avg 是加权平均，要走完整条 position 历史。
/// 所以这个函数接收**已 reduce 过的 position-level 数据**。caller 一般会用
/// `pipeline::account::valuation::compute_realized_pnl_per_position(events)` 之类的复合函数。
///
/// 第一版直接放 pipeline 里实现，这里只暴露 cash_delta 工具。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::account::types::{CloseReason, EventSource, PositionEvent, PositionId};
    use crate::domain::shared::{OccurredAt, Shares};

    fn make_event(kind: PositionEventKind) -> PositionEvent {
        PositionEvent {
            id: "test".into(),
            position_id: PositionId::from_string("p1".into()),
            kind,
            occurred_at: OccurredAt::now(),
            source: EventSource::Manual,
            agent_note_md: String::new(),
        }
    }

    #[test]
    fn opened_event_subtracts_cost_and_commission() {
        let kind = PositionEventKind::Opened {
            entry_price: Yuan::new(10.0).unwrap(),
            shares: Shares::new(100).unwrap(),
            commission: Yuan::new(5.0).unwrap(),
        };
        let delta = single_event_cash_delta(&kind);
        // -10 × 100 - 5 = -1005
        assert!((delta - (-1005.0)).abs() < 1e-6);
    }

    #[test]
    fn closed_event_adds_proceeds_minus_fees() {
        let kind = PositionEventKind::Closed {
            exit_price: Yuan::new(12.0).unwrap(),
            shares: Shares::new(100).unwrap(),
            reason: CloseReason::Manual,
            commission: Yuan::new(5.0).unwrap(),
            stamp_tax: Yuan::new(1.2).unwrap(),
        };
        let delta = single_event_cash_delta(&kind);
        // 12 × 100 - 5 - 1.2 = 1193.8
        assert!((delta - 1193.8).abs() < 1e-6);
    }

    #[test]
    fn scaled_in_subtracts_cost() {
        let kind = PositionEventKind::ScaledIn {
            delta: Shares::new(200).unwrap(),
            price: Yuan::new(11.0).unwrap(),
            new_avg: Yuan::new(10.7).unwrap(),
            commission: Yuan::new(5.0).unwrap(),
        };
        let d = single_event_cash_delta(&kind);
        // -11 × 200 - 5 = -2205
        assert!((d - (-2205.0)).abs() < 1e-6);
    }

    #[test]
    fn scaled_out_adds_proceeds_minus_fees() {
        let kind = PositionEventKind::ScaledOut {
            delta: Shares::new(100).unwrap(),
            price: Yuan::new(11.5).unwrap(),
            commission: Yuan::new(5.0).unwrap(),
            stamp_tax: Yuan::new(1.15).unwrap(),
        };
        let d = single_event_cash_delta(&kind);
        // 11.5 × 100 - 5 - 1.15 = 1143.85
        assert!((d - 1143.85).abs() < 1e-6);
    }

    #[test]
    fn stops_adjusted_no_cash_impact() {
        let kind = PositionEventKind::StopsAdjusted {
            stop_loss: Some(Yuan::new(10.0).unwrap()),
            take_profit: None,
            time_stop_at: None,
        };
        assert_eq!(single_event_cash_delta(&kind), 0.0);
    }

    #[test]
    fn full_open_close_cycle() {
        // 开 100 股 @ 10 → 平 100 股 @ 12
        // open: -1005 (cost 1000 + commission 5)
        // close: 1200 - 5 commission - 1.2 stamp = 1193.8
        // net: 188.8（盈利约 200 - 11.2 费 = 188.8）
        let events = vec![
            make_event(PositionEventKind::Opened {
                entry_price: Yuan::new(10.0).unwrap(),
                shares: Shares::new(100).unwrap(),
                commission: Yuan::new(5.0).unwrap(),
            }),
            make_event(PositionEventKind::Closed {
                exit_price: Yuan::new(12.0).unwrap(),
                shares: Shares::new(100).unwrap(),
                reason: CloseReason::Manual,
                commission: Yuan::new(5.0).unwrap(),
                stamp_tax: Yuan::new(1.2).unwrap(),
            }),
        ];
        let total = reduce_events_to_cash_delta(&events);
        assert!((total.value() - 188.8).abs() < 1e-6);
    }
}
