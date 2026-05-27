//! AgentMessage 持久化对话 / context 消息。
//!
//! Spec: docs/design/agent-infra-module.md §2 `AgentMessage`
//!
//! 不变量：
//! - role / block 组合必须按 spec §2 表格校验。
//! - 图片使用 `dataRef` 指向本地附件 / 缓存，不把大二进制塞进消息表。
//! - thinking 是否持久化取决于 provider；跨 provider 不保证恢复。

use crate::domain::shared::OccurredAt;
use serde::{Deserialize, Serialize};
use specta::Type;

/// 通用 JSON summary，对应 spec §2 `JsonSummary`。
pub type JsonSummary = serde_json::Value;

/// AgentMessage 角色枚举。
///
/// Spec: agent-infra-module.md §2
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum AgentMessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// AgentMessage block 类型。
///
/// Spec: agent-infra-module.md §2 `AgentMessageBlock`
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessageBlock {
    Text {
        text: String,
    },
    Image {
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(rename = "dataRef")]
        data_ref: String,
    },
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    ToolUse {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        name: String,
        #[serde(rename = "inputSummary")]
        input_summary: JsonSummary,
    },
    ToolResult {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "outputSummary")]
        output_summary: JsonSummary,
        #[serde(rename = "isError")]
        is_error: bool,
    },
}

impl AgentMessageBlock {
    /// 当前 block 是否允许在给定 role 下出现（spec §2 表格）。
    pub fn allowed_for_role(&self, role: AgentMessageRole) -> bool {
        match (role, self) {
            (AgentMessageRole::System, AgentMessageBlock::Text { .. }) => true,
            (
                AgentMessageRole::User,
                AgentMessageBlock::Text { .. } | AgentMessageBlock::Image { .. },
            ) => true,
            (
                AgentMessageRole::Assistant,
                AgentMessageBlock::Text { .. }
                | AgentMessageBlock::Thinking { .. }
                | AgentMessageBlock::ToolUse { .. },
            ) => true,
            (AgentMessageRole::Tool, AgentMessageBlock::ToolResult { .. }) => true,
            _ => false,
        }
    }
}

/// 一条 Agent 消息（context / 持久化对话历史）。
///
/// Spec: agent-infra-module.md §2 `AgentMessage`
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessage {
    pub message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub role: AgentMessageRole,
    pub blocks: Vec<AgentMessageBlock>,
    pub created_at: OccurredAt,
}

#[derive(Debug, thiserror::Error)]
#[error("role {role:?} disallows block index {index}")]
pub struct MessageRoleBlockError {
    pub role: AgentMessageRole,
    pub index: usize,
}

impl AgentMessage {
    /// 按 spec §2 表格校验 role / block.type 组合。
    pub fn validate_role_blocks(&self) -> Result<(), MessageRoleBlockError> {
        for (i, b) in self.blocks.iter().enumerate() {
            if !b.allowed_for_role(self.role) {
                return Err(MessageRoleBlockError {
                    role: self.role,
                    index: i,
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn msg(role: AgentMessageRole, blocks: Vec<AgentMessageBlock>) -> AgentMessage {
        AgentMessage {
            message_id: "m1".into(),
            run_id: Some("r1".into()),
            role,
            blocks,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn role_block_allowed_combinations() {
        assert!(msg(
            AgentMessageRole::System,
            vec![AgentMessageBlock::Text { text: "x".into() }]
        )
        .validate_role_blocks()
        .is_ok());
        assert!(msg(
            AgentMessageRole::User,
            vec![AgentMessageBlock::Image {
                mime_type: "image/png".into(),
                data_ref: "ref://x".into(),
            }]
        )
        .validate_role_blocks()
        .is_ok());
        assert!(msg(
            AgentMessageRole::Assistant,
            vec![
                AgentMessageBlock::Thinking { text: "t".into(), provider: None },
                AgentMessageBlock::ToolUse {
                    tool_call_id: "tc1".into(),
                    name: "x".into(),
                    input_summary: serde_json::json!({"k":1})
                }
            ]
        )
        .validate_role_blocks()
        .is_ok());
        assert!(msg(
            AgentMessageRole::Tool,
            vec![AgentMessageBlock::ToolResult {
                tool_call_id: "tc1".into(),
                output_summary: serde_json::json!("ok"),
                is_error: false,
            }]
        )
        .validate_role_blocks()
        .is_ok());
    }

    #[test]
    fn role_block_disallowed_combinations() {
        // system 不允许 image
        assert!(msg(
            AgentMessageRole::System,
            vec![AgentMessageBlock::Image {
                mime_type: "image/png".into(),
                data_ref: "r".into()
            }]
        )
        .validate_role_blocks()
        .is_err());
        // user 不允许 thinking
        assert!(msg(
            AgentMessageRole::User,
            vec![AgentMessageBlock::Thinking { text: "x".into(), provider: None }]
        )
        .validate_role_blocks()
        .is_err());
        // assistant 不允许 tool_result
        assert!(msg(
            AgentMessageRole::Assistant,
            vec![AgentMessageBlock::ToolResult {
                tool_call_id: "x".into(),
                output_summary: serde_json::json!(null),
                is_error: false,
            }]
        )
        .validate_role_blocks()
        .is_err());
        // tool 不允许 text
        assert!(msg(
            AgentMessageRole::Tool,
            vec![AgentMessageBlock::Text { text: "x".into() }]
        )
        .validate_role_blocks()
        .is_err());
    }

    #[test]
    fn agent_message_serde_roundtrip() {
        let m = msg(
            AgentMessageRole::Assistant,
            vec![
                AgentMessageBlock::Text { text: "hi".into() },
                AgentMessageBlock::ToolUse {
                    tool_call_id: "tc1".into(),
                    name: "fetch_quote".into(),
                    input_summary: serde_json::json!({"tsCode":"600519.SH"}),
                },
            ],
        );
        let j = serde_json::to_string(&m).unwrap();
        let back: AgentMessage = serde_json::from_str(&j).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn message_block_tagged_serialization() {
        let b = AgentMessageBlock::Image {
            mime_type: "image/jpeg".into(),
            data_ref: "ref://abc".into(),
        };
        let j = serde_json::to_value(&b).unwrap();
        assert_eq!(j["type"], "image");
        assert_eq!(j["mimeType"], "image/jpeg");
        assert_eq!(j["dataRef"], "ref://abc");
    }
}
