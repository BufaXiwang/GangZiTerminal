//! Canonical wire shape——内部消息以 Anthropic content-block 形态作为最具表达力的超集
//! （text / thinking / image / tool_use / tool_result）。其他 provider 需要把自己的协议
//! 翻译到这里。所有结构 serde 双向，可直接落库 / 通过 Tauri emit 给前端。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 消息角色。System 单独走 `AgentRequest::system`，messages 列表里只放 user/assistant。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// 一条消息由若干 content block 组成。这是 canonical 的中间表示——
/// AnthropicProvider 序列化时直接对应 `content: [...]`；OpenAIProvider 把这套结构降级翻译。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Block>,
}

/// content block 的全部类型。`#[serde(tag = "type")]` 让 JSON 形态贴近 Anthropic 协议。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    /// 普通文本片段。
    Text {
        text: String,
        /// cache_control 默认 false；只在打 cache breakpoint 的最后一个 block 上置 true。
        #[serde(skip_serializing_if = "is_false", default)]
        cache_control: bool,
    },

    /// Extended thinking 块（Anthropic）。signature 由 provider 写入，
    /// 跨轮次回传时必须原样带回，否则签名校验会失败。
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },

    /// Redacted thinking——被服务器加密的思考块。loop 必须原样转发，不能丢。
    RedactedThinking { data: String },

    /// 图片。data 是 base64，mime 形如 image/png / image/jpeg。
    Image { mime: String, data: String },

    /// Agent 调本地工具。loop 看到此 block 应执行 ToolRegistry.dispatch，
    /// 把结果作为 ToolResult 拼回下一轮 messages。
    /// `server_side=true` 表示这是 provider 替我们执行的（Anthropic web_search_20250305 等）——
    /// loop 不要执行，等 provider 在同一回合内回填 tool_result 即可。
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(skip_serializing_if = "is_false", default)]
        server_side: bool,
    },

    /// 工具执行结果。`server_side=true` 时 content 由 provider 填充并原样转发。
    ToolResult {
        tool_use_id: String,
        content: Vec<ToolResultContent>,
        #[serde(skip_serializing_if = "is_false", default)]
        is_error: bool,
        #[serde(skip_serializing_if = "is_false", default)]
        server_side: bool,
        /// 同 Block::Text 的语义。
        #[serde(skip_serializing_if = "is_false", default)]
        cache_control: bool,
    },
}

/// tool_result 的 content 通常是文本，偶尔是图（截图工具）。
/// Anthropic 也允许嵌套结构化对象（server-side tool 的原始返回），用 Json 兜底。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContent {
    Text {
        text: String,
    },
    Image {
        mime: String,
        data: String,
    },
    /// 对 server-side 工具（如 Anthropic web_search_tool_result）原样透传。
    /// 字段名带 _raw 提示这是绕过 canonical 抽象的逃生舱。
    Json {
        raw: Value,
    },
}

/// system 字段的 block。多段拼接，按位置打 cache_control 形成 cache prefix 链。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemBlock {
    pub text: String,
    /// 在该 block 末尾打 cache breakpoint。整次请求最多 4 个 breakpoint
    /// （含 tools 区与 messages 区里的），超过会被 provider 拒绝。
    #[serde(skip_serializing_if = "is_false", default)]
    pub cache_control: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
pub(super) fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn block_text_round_trips() {
        let block = Block::Text {
            text: "hi".into(),
            cache_control: true,
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(v["type"], "text");
        assert_eq!(v["text"], "hi");
        assert_eq!(v["cache_control"], true);
        let back: Block = serde_json::from_value(v).unwrap();
        match back {
            Block::Text {
                text,
                cache_control,
            } => {
                assert_eq!(text, "hi");
                assert!(cache_control);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn block_text_omits_default_cache_control() {
        let block = Block::Text {
            text: "hi".into(),
            cache_control: false,
        };
        let v = serde_json::to_value(&block).unwrap();
        assert!(
            v.get("cache_control").is_none(),
            "default cache_control 应该不序列化，避免污染请求体"
        );
    }

    #[test]
    fn tool_use_with_server_side_flag() {
        let block = Block::ToolUse {
            id: "toolu_1".into(),
            name: "web_search".into(),
            input: json!({"query": "茅台"}),
            server_side: true,
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(v["type"], "tool_use");
        assert_eq!(v["server_side"], true);
    }

    #[test]
    fn message_serializes_with_role_lowercase() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![Block::Text {
                text: "ok".into(),
                cache_control: false,
            }],
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["role"], "assistant");
    }
}
