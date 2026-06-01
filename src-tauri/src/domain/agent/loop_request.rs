//! Loop request / run summary / token estimate 类型。
//!
//! Spec: docs/design/agent-infra-module.md §5 Infra Loop API
//!
//! Runtime 通过这些类型把一次 run 的配置传给 Infra，再读 `RunSummary` 关闭审计循环。

use serde::{Deserialize, Serialize};
use specta::Type;

use super::channel::ProviderChannel;
use super::events::AgentStopReason;
use super::messages::AgentMessage;

/// 单次 Agent run 的执行参数。
///
/// Spec: agent-infra-module.md §5 Infra Loop API
///
/// 注：spec §5 明确"request 不含 server-side tool 字段"——所有 chat-completable provider
/// 都通过 Skill 文本协议提供工具能力。
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunRequest {
    pub run_id: String,
    pub trigger: String,
    pub channel: ProviderChannel,
    pub max_turns: u32,
    /// 把已有聊天历史 / 续接消息一起带入；Infra 不负责拉取历史。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seed_messages: Vec<AgentMessage>,
}

/// 一次 run 完成后的总结，供 Runtime 关闭审计。
///
/// Spec: agent-infra-module.md §5
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub run_id: String,
    pub stop_reason: AgentStopReason,
    pub turns: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u32>,
    /// 本次产生的 skill_call_id 列表（Runtime 据此找审计）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_call_ids: Vec<String>,
}

impl RunSummary {
    pub fn empty(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            stop_reason: AgentStopReason::Completed,
            turns: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: None,
            cache_write_tokens: None,
            skill_call_ids: vec![],
        }
    }
}

/// token 估算输出。
///
/// Spec: agent-infra-module.md §5 `estimate_context_tokens`
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TokenEstimate {
    pub total_tokens: u32,
    /// 是否超过 channel 软限（caller 决定是否触发压缩）。
    pub over_soft_limit: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::channel::{ProviderChannel, WireFormat};

    #[test]
    fn agent_run_request_serde_roundtrip() {
        let r = AgentRunRequest {
            run_id: "r1".into(),
            trigger: "user_message".into(),
            channel: ProviderChannel {
                channel_id: "x".into(),
                provider: "anthropic".into(),
                wire_format: WireFormat::Messages,
                base_url: None,
                api_key: String::new(),
                model: "claude".into(),
                stream: true,
                enabled: true,
                supports_vision: false,
                supports_thinking: false,
                max_output_tokens: None,
                context_window_tokens: None,
                thinking_budget_tokens: None,
            },
            max_turns: 12,
            seed_messages: vec![],
        };
        let j = serde_json::to_string(&r).unwrap();
        let back: AgentRunRequest = serde_json::from_str(&j).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn run_summary_default_empty() {
        let s = RunSummary::empty("r1");
        assert_eq!(s.turns, 0);
        assert_eq!(s.input_tokens, 0);
        assert_eq!(s.stop_reason, AgentStopReason::Completed);
    }
}
