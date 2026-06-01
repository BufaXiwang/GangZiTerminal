//! AgentMessage 持久化对话 / context 消息。
//!
//! Spec: docs/design/agent-infra-module.md §2 `AgentMessage`
//!
//! 不变量：
//! - role / block 组合必须按 spec §2 表格校验。
//! - Skill 调用 / 结果以 XML 标签嵌在 `text` block 中，**不**作为独立 block type。
//! - 图片 `dataRef` 是 PayloadStore URI（`payload://pl_xxx`）或 `file:///`；不是 base64 数据。
//! - thinking 是否持久化取决于 provider；Anthropic 等需要保留 provider-specific metadata（signature）。

use crate::domain::shared::OccurredAt;
use serde::{Deserialize, Serialize};
use specta::Type;

/// 通用 JSON summary，对应 spec §2 `JsonSummary`。
pub type JsonSummary = serde_json::Value;

/// AgentMessage 角色枚举。
///
/// Spec: agent-infra-module.md §2
///
/// 注：`tool` role 不存在；skill_result 以 `user` role + text block（含 `<skill_result>` XML）形式回写。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum AgentMessageRole {
    System,
    User,
    Assistant,
}

/// AgentMessage block 类型。
///
/// Spec: agent-infra-module.md §2 `AgentMessageBlock`
///
/// Skill 调用 / 结果（`<use_skill>` / `<skill_result>` / `<skill_error>`）嵌在 text block 中。
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessageBlock {
    Text {
        text: String,
    },
    Image {
        #[serde(rename = "mimeType")]
        mime_type: String,
        /// PayloadStore URI（`payload://pl_xxx`）或 `file:///path`。Provider adapter 在
        /// build wire 时 dereference → base64 编码。
        #[serde(rename = "dataRef")]
        data_ref: String,
    },
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        /// Provider-specific metadata（如 Anthropic extended thinking 的 `signature` / `redacted`）。
        /// Adapter 在 build wire 时把这里的内容还原到 provider wire format。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
}

/// 消息种类（spec §2 `AgentMessage.kind`）。
///
/// Spec: agent-infra-module.md §2
/// - `Chat`（默认）= 普通对话消息。
/// - `Summary` = §4 Summarize 产出的压缩检查点；durable，不再被 MicroClear / Drop / 再次 Summarize 触碰。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type, Default)]
#[serde(rename_all = "camelCase")]
pub enum MessageKind {
    #[default]
    Chat,
    Summary,
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
                AgentMessageBlock::Text { .. } | AgentMessageBlock::Thinking { .. },
            ) => true,
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
    /// 多轮会话标识（Runtime 提供）；跨 run 续接靠它分组（spec §2）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    /// 会话内单调序号；持久化排序用（spec §2）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
    /// 消息种类：默认 chat；summary = 压缩检查点（durable，不再被压缩）（spec §2）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<MessageKind>,
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
            conversation_id: None,
            seq: None,
            kind: None,
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
                data_ref: "payload://pl_x".into(),
            }]
        )
        .validate_role_blocks()
        .is_ok());
        assert!(msg(
            AgentMessageRole::Assistant,
            vec![
                AgentMessageBlock::Thinking {
                    text: "t".into(),
                    provider: None,
                    metadata: None,
                },
                AgentMessageBlock::Text {
                    text: r#"<use_skill name="fetch_quote">{"tsCode":"600519.SH"}</use_skill>"#.into()
                },
            ]
        )
        .validate_role_blocks()
        .is_ok());
        // skill_result lives in user-role text block
        assert!(msg(
            AgentMessageRole::User,
            vec![AgentMessageBlock::Text {
                text: r#"<skill_result name="fetch_quote" call_id="sc_1">{"price":"1.0"}</skill_result>"#.into()
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
            vec![AgentMessageBlock::Thinking {
                text: "x".into(),
                provider: None,
                metadata: None
            }]
        )
        .validate_role_blocks()
        .is_err());
        // assistant 不允许 image
        assert!(msg(
            AgentMessageRole::Assistant,
            vec![AgentMessageBlock::Image {
                mime_type: "image/png".into(),
                data_ref: "r".into()
            }]
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
                AgentMessageBlock::Thinking {
                    text: "reasoning".into(),
                    provider: Some("anthropic".into()),
                    metadata: Some(serde_json::json!({"signature": "abc"})),
                },
            ],
        );
        let j = serde_json::to_string(&m).unwrap();
        let back: AgentMessage = serde_json::from_str(&j).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn thinking_metadata_preserves_signature() {
        let b = AgentMessageBlock::Thinking {
            text: "think".into(),
            provider: Some("anthropic".into()),
            metadata: Some(serde_json::json!({"signature": "sig-xyz", "redacted": false})),
        };
        let j = serde_json::to_value(&b).unwrap();
        assert_eq!(j["type"], "thinking");
        assert_eq!(j["metadata"]["signature"], "sig-xyz");
    }

    #[test]
    fn message_block_image_serialization() {
        let b = AgentMessageBlock::Image {
            mime_type: "image/jpeg".into(),
            data_ref: "payload://pl_abc".into(),
        };
        let j = serde_json::to_value(&b).unwrap();
        assert_eq!(j["type"], "image");
        assert_eq!(j["mimeType"], "image/jpeg");
        assert_eq!(j["dataRef"], "payload://pl_abc");
    }
}
