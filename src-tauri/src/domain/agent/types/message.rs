//! `AgentMessage` 与 4 角色模型 —— spec `agent-infra-module.md §2`。
//!
//! spec 定义的 AgentMessage 是 Agent Infra 对外稳定的消息契约，含 messageId /
//! runId / role / blocks / createdAt。Pipeline 内部的 wire `Message` 是
//! Anthropic 形态的 canonical 序列化结构（user/assistant + tool_use/tool_result
//! 嵌入 user message）；本类型是 domain layer 的稳定 contract，前端和 Runtime
//! 通过它读历史 / 重放消息。
//!
//! 角色 ↔ block 约束矩阵（spec §2）：
//! - `system` 只允许 `text`
//! - `user` 只允许 `text` / `image`
//! - `assistant` 只允许 `text` / `thinking` / `tool_use`
//! - `tool` 只允许 `tool_result`
//!
//! `validate()` 强制该矩阵；调用方违反时返回 `InvalidRoleBlock`。

use serde::{Deserialize, Serialize};

use super::wire::Block;

/// spec §2 角色 4 值闭集合。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

impl MessageRole {
    pub fn as_str(self) -> &'static str {
        match self {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        }
    }
}

/// spec §2 `AgentMessage` —— Agent Infra 对外的稳定消息契约。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessage {
    pub message_id: String,
    pub run_id: String,
    pub role: MessageRole,
    pub blocks: Vec<Block>,
    pub created_at: String,
}

/// spec §2 角色↔block 矩阵违规。
#[derive(Debug, Clone)]
pub struct InvalidRoleBlock {
    pub role: MessageRole,
    pub block_type: &'static str,
}

impl std::fmt::Display for InvalidRoleBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "role={} 不允许包含 block 类型 `{}`（spec agent-infra-module.md §2 角色矩阵）",
            self.role.as_str(),
            self.block_type
        )
    }
}

impl std::error::Error for InvalidRoleBlock {}

fn block_kind(b: &Block) -> &'static str {
    match b {
        Block::Text { .. } => "text",
        Block::Thinking { .. } => "thinking",
        Block::RedactedThinking { .. } => "redacted_thinking",
        Block::Image { .. } => "image",
        Block::ToolUse { .. } => "tool_use",
        Block::ToolResult { .. } => "tool_result",
    }
}

fn is_allowed(role: MessageRole, b: &Block) -> bool {
    match role {
        MessageRole::System => matches!(b, Block::Text { .. }),
        MessageRole::User => matches!(b, Block::Text { .. } | Block::Image { .. }),
        MessageRole::Assistant => matches!(
            b,
            Block::Text { .. }
                | Block::Thinking { .. }
                | Block::RedactedThinking { .. }
                | Block::ToolUse { .. }
        ),
        MessageRole::Tool => matches!(b, Block::ToolResult { .. }),
    }
}

impl AgentMessage {
    /// 校验角色↔block 矩阵；违反返回首个不允许的 block 类型。
    pub fn validate(&self) -> Result<(), InvalidRoleBlock> {
        for b in &self.blocks {
            if !is_allowed(self.role, b) {
                return Err(InvalidRoleBlock {
                    role: self.role,
                    block_type: block_kind(b),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(role: MessageRole, blocks: Vec<Block>) -> AgentMessage {
        AgentMessage {
            message_id: "m1".into(),
            run_id: "r1".into(),
            role,
            blocks,
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn system_only_allows_text() {
        assert!(msg(MessageRole::System, vec![Block::Text {
            text: "hi".into(),
            cache_control: false,
        }])
        .validate()
        .is_ok());
        let err = msg(
            MessageRole::System,
            vec![Block::Image {
                mime: "image/png".into(),
                data: "x".into(),
            }],
        )
        .validate()
        .unwrap_err();
        assert_eq!(err.block_type, "image");
    }

    #[test]
    fn tool_only_allows_tool_result() {
        let ok = msg(
            MessageRole::Tool,
            vec![Block::ToolResult {
                tool_use_id: "t1".into(),
                content: vec![],
                is_error: false,
                server_side: false,
                cache_control: false,
            }],
        );
        assert!(ok.validate().is_ok());
        let bad = msg(
            MessageRole::Tool,
            vec![Block::Text { text: "x".into(), cache_control: false }],
        );
        assert_eq!(bad.validate().unwrap_err().block_type, "text");
    }

    #[test]
    fn assistant_disallows_image_and_tool_result() {
        let bad = msg(
            MessageRole::Assistant,
            vec![Block::ToolResult {
                tool_use_id: "t1".into(),
                content: vec![],
                is_error: false,
                server_side: false,
                cache_control: false,
            }],
        );
        assert_eq!(bad.validate().unwrap_err().block_type, "tool_result");
    }

    #[test]
    fn assistant_allows_thinking_and_tool_use() {
        let ok = msg(
            MessageRole::Assistant,
            vec![
                Block::Thinking { thinking: "x".into(), signature: None },
                Block::ToolUse {
                    id: "t1".into(),
                    name: "n".into(),
                    input: json!({}),
                    server_side: false,
                },
            ],
        );
        assert!(ok.validate().is_ok());
    }
}
