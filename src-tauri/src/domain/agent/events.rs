//! AgentEvent — Agent loop 流式事件。
//!
//! Spec: docs/design/agent-infra-module.md §2 `AgentEvent`，§4 compaction event
//!
//! Runtime / 前端通过 `AgentEvent` 订阅 loop 状态；Infra emit。

use crate::domain::shared::ErrorCode;
use serde::{Deserialize, Serialize};
use specta::Type;

use super::messages::JsonSummary;

/// Agent loop 终止原因。
///
/// Spec: agent-infra-module.md §2 `AgentStopReason`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum AgentStopReason {
    Completed,
    MaxTurns,
    Cancelled,
    ProviderStop,
    SkillError,
    ContextLimit,
    Error,
}

/// Statistics-level token usage report from provider.
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageBreakdown {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u32>,
}

/// Compaction tier — 与 spec §4 压缩顺序对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum CompactedTier {
    MicroClear,
    Summarize,
    Drop,
    ReactiveRetry,
}

/// Agent loop 统一事件。
///
/// Spec: agent-infra-module.md §2 `AgentEvent`
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    RunStart {
        #[serde(rename = "runId")]
        run_id: String,
        trigger: String,
        model: String,
    },
    TextDelta {
        #[serde(rename = "runId")]
        run_id: String,
        delta: String,
    },
    ThinkingDelta {
        #[serde(rename = "runId")]
        run_id: String,
        delta: String,
    },
    SkillStart {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "skillCallId")]
        skill_call_id: String,
        name: String,
        #[serde(rename = "inputSummary")]
        input_summary: JsonSummary,
    },
    SkillEnd {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "skillCallId")]
        skill_call_id: String,
        name: String,
        #[serde(rename = "outputSummary")]
        output_summary: JsonSummary,
        #[serde(rename = "isError")]
        is_error: bool,
        #[serde(rename = "durationMs")]
        duration_ms: u64,
    },
    Compacted {
        #[serde(rename = "runId")]
        run_id: String,
        tier: CompactedTier,
        #[serde(rename = "droppedMessages")]
        dropped_messages: u32,
        #[serde(
            rename = "estimatedTokensSaved",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        estimated_tokens_saved: Option<u32>,
    },
    Usage {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "inputTokens")]
        input_tokens: u32,
        #[serde(rename = "outputTokens")]
        output_tokens: u32,
        #[serde(
            rename = "cacheReadTokens",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        cache_read_tokens: Option<u32>,
        #[serde(
            rename = "cacheWriteTokens",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        cache_write_tokens: Option<u32>,
    },
    Done {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "stopReason")]
        stop_reason: AgentStopReason,
        turns: u32,
    },
    /// 错误事件。Spec §2: `error.code` 必填，取自 shared `ErrorCode` 封闭集合。
    Error {
        #[serde(rename = "runId")]
        run_id: String,
        code: ErrorCode,
        message: String,
    },
}

impl AgentEvent {
    pub fn run_id(&self) -> &str {
        match self {
            AgentEvent::RunStart { run_id, .. }
            | AgentEvent::TextDelta { run_id, .. }
            | AgentEvent::ThinkingDelta { run_id, .. }
            | AgentEvent::SkillStart { run_id, .. }
            | AgentEvent::SkillEnd { run_id, .. }
            | AgentEvent::Compacted { run_id, .. }
            | AgentEvent::Usage { run_id, .. }
            | AgentEvent::Done { run_id, .. }
            | AgentEvent::Error { run_id, .. } => run_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_event_text_delta_serde_roundtrip() {
        let e = AgentEvent::TextDelta {
            run_id: "r1".into(),
            delta: "hi".into(),
        };
        let j = serde_json::to_string(&e).unwrap();
        assert!(j.contains("\"type\":\"text_delta\""));
        let back: AgentEvent = serde_json::from_str(&j).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn agent_event_compacted_uses_snake_case_tier() {
        let e = AgentEvent::Compacted {
            run_id: "r1".into(),
            tier: CompactedTier::ReactiveRetry,
            dropped_messages: 3,
            estimated_tokens_saved: Some(1200),
        };
        let j = serde_json::to_value(&e).unwrap();
        assert_eq!(j["type"], "compacted");
        assert_eq!(j["tier"], "reactive_retry");
        assert_eq!(j["droppedMessages"], 3);
        assert_eq!(j["estimatedTokensSaved"], 1200);
    }

    #[test]
    fn agent_event_done_stop_reason() {
        let e = AgentEvent::Done {
            run_id: "r1".into(),
            stop_reason: AgentStopReason::MaxTurns,
            turns: 8,
        };
        let j = serde_json::to_value(&e).unwrap();
        assert_eq!(j["stopReason"], "max_turns");
    }

    #[test]
    fn agent_event_error_requires_code() {
        let e = AgentEvent::Error {
            run_id: "r1".into(),
            code: ErrorCode::ProviderContextTooLong,
            message: "context too long".into(),
        };
        let j = serde_json::to_value(&e).unwrap();
        assert_eq!(j["type"], "error");
        assert_eq!(j["code"], "provider_context_too_long");
        assert_eq!(j["message"], "context too long");
    }

    #[test]
    fn agent_event_skill_start_and_end_serialize() {
        let s = AgentEvent::SkillStart {
            run_id: "r1".into(),
            skill_call_id: "sc_1".into(),
            name: "fetch_quote".into(),
            input_summary: serde_json::json!({"tsCode": "600519.SH"}),
        };
        let j = serde_json::to_value(&s).unwrap();
        assert_eq!(j["type"], "skill_start");
        assert_eq!(j["skillCallId"], "sc_1");
        let e = AgentEvent::SkillEnd {
            run_id: "r1".into(),
            skill_call_id: "sc_1".into(),
            name: "fetch_quote".into(),
            output_summary: serde_json::json!({"ok": true}),
            is_error: false,
            duration_ms: 30,
        };
        let j = serde_json::to_value(&e).unwrap();
        assert_eq!(j["type"], "skill_end");
        assert_eq!(j["durationMs"], 30);
    }

    #[test]
    fn agent_event_run_id_helper() {
        let e = AgentEvent::Error {
            run_id: "rZ".into(),
            code: ErrorCode::ParseError,
            message: "boom".into(),
        };
        assert_eq!(e.run_id(), "rZ");
    }
}
