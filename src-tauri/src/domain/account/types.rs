//! Backward-compatible account type facade.
//!
//! 新代码优先从 `domain::account::{Position, PositionEvent, AccountSnapshot, ...}`
//! 引入；这个文件保留给已有 `domain::account::types::*` 调用点。

pub use super::events::{EventSource, PositionEvent, PositionEventKind, PositionSignalKind};
pub use super::position::{CloseReason, Position, PositionId, PositionStatus, Side};
pub use super::snapshot::AccountSnapshot;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shared::{OccurredAt, Shares, Yuan};

    #[test]
    fn position_status_open_vs_closed() {
        let open = PositionStatus::Open;
        let closed = PositionStatus::Closed {
            exit_price: Yuan::new(12.0).unwrap(),
            exit_at: OccurredAt::now(),
            reason: CloseReason::TakeProfit,
        };
        assert!(open.is_open());
        assert!(!open.is_closed());
        assert!(closed.is_closed());
        assert!(!closed.is_open());
    }

    #[test]
    fn close_reason_as_str_stable() {
        assert_eq!(CloseReason::Manual.as_str(), "manual");
        assert_eq!(CloseReason::StopLoss.as_str(), "stop_loss");
        assert_eq!(CloseReason::TakeProfit.as_str(), "take_profit");
        assert_eq!(CloseReason::TimeStop.as_str(), "time_stop");
        assert_eq!(CloseReason::Invalidated.as_str(), "invalidated");
    }

    #[test]
    fn event_kind_tags() {
        let opened = PositionEventKind::Opened {
            entry_price: Yuan::new(11.5).unwrap(),
            shares: Shares::new(100).unwrap(),
            commission: Yuan::new(5.0).unwrap(),
        };
        assert_eq!(opened.tag(), "opened");

        let stops = PositionEventKind::StopsAdjusted {
            stop_loss: Some(Yuan::new(10.0).unwrap()),
            take_profit: None,
            time_stop_at: None,
        };
        assert_eq!(stops.tag(), "stops_adjusted");
    }

    #[test]
    fn position_id_unique() {
        let a = PositionId::new();
        let b = PositionId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn event_kind_serde_round_trip() {
        let kind = PositionEventKind::Opened {
            entry_price: Yuan::new(11.5).unwrap(),
            shares: Shares::new(200).unwrap(),
            commission: Yuan::new(5.0).unwrap(),
        };
        let json = serde_json::to_string(&kind).unwrap();
        assert!(json.contains("\"kind\":\"opened\""));
        let back: PositionEventKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, back);
    }

    #[test]
    fn event_source_serde_round_trip() {
        let src = EventSource::Briefing {
            analysis_id: "abc-123".into(),
        };
        let json = serde_json::to_string(&src).unwrap();
        assert!(json.contains("\"kind\":\"briefing\""));
        assert!(json.contains("\"analysis_id\":\"abc-123\""));
        let back: EventSource = serde_json::from_str(&json).unwrap();
        assert_eq!(src, back);
    }

    #[test]
    fn position_status_serde() {
        let closed = PositionStatus::Closed {
            exit_price: Yuan::new(12.5).unwrap(),
            exit_at: OccurredAt::new(1747200000000),
            reason: CloseReason::TakeProfit,
        };
        let json = serde_json::to_string(&closed).unwrap();
        assert!(json.contains("\"state\":\"closed\""));
        let back: PositionStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(closed, back);
    }
}
