//! Agent Infra 前端事件名常量 + envelope helper。
//!
//! Spec: docs/design/agent-infra-module.md §2 `AgentEvent`，shared-types.md §6 (`AppEventEnvelope`)
//!
//! 前端通过 `listen('agent-event', ...)` 订阅 loop 事件流。同一 channel 用单 event name；
//! 多 run 在 payload 内通过 `runId` 区分。

use crate::domain::agent::AgentEvent;
use crate::domain::shared::AppEventEnvelope;
use chrono::Utc;
use uuid::Uuid;

/// 前端 `listen('agent-event', ...)` 使用的事件类型。
pub const AGENT_EVENT: &str = "agent-event";

pub fn wrap_agent_event(
    payload: AgentEvent,
    correlation_id: Option<String>,
) -> AppEventEnvelope<AgentEvent> {
    AppEventEnvelope {
        event_id: Uuid::new_v4().to_string(),
        event_type: AGENT_EVENT.to_string(),
        occurred_at: Utc::now(),
        correlation_id,
        causation_id: None,
        payload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentStopReason;

    #[test]
    fn envelope_wraps_payload_and_sets_event_type() {
        let e = AgentEvent::Done {
            run_id: "r1".into(),
            stop_reason: AgentStopReason::Completed,
            turns: 1,
        };
        let env = wrap_agent_event(e.clone(), Some("corr-1".into()));
        assert_eq!(env.event_type, AGENT_EVENT);
        assert_eq!(env.correlation_id.as_deref(), Some("corr-1"));
        assert_eq!(env.payload, e);
    }
}
