//! Chat history 形态互转——chat_messages 表的 JSON 行 ⇄ agent 的 `Vec<Message>`。
//!
//! 设计要点：
//! - **结构化优先**：assistant 落库时把 `Vec<Block>`（含 tool_use / image / text）
//!   序列化进 `content_json.blocks`；user 同理（图片 + 文本拆 block）。
//!   下次 chat 加载时直接反序列化恢复多轮 messages 结构，不丢工具调用上下文。
//! - **向后兼容**：老消息没有 `content_json.blocks`——回退用 `content_md` 拼一条
//!   `Block::Text`。这样新旧库都能跑，无需迁移。
//! - **compact_boundary 优先**：`read_recent_chat_thread` 找最新的 `kind =
//!   'compact_boundary'` 行——若存在，只取该行**之后**的真实对话作为结构化历史，
//!   并把 boundary 自身的摘要文本作为第一条 user message 注入；否则拿最新 N 条。
//!
//! 这层 helper 是 chat pipeline 唯一从 DB 反序列化历史的入口。

use crate::domain::agent::types::{Block, Message, Role};
use serde_json::{json, Value};
use tauri::AppHandle;

/// chat_messages 行里允许的 kind——和 DB schema CHECK 约束保持一致。
/// 当前 history 只显式 match 'chat' 和 'compact_boundary'；其他 kind 在 DB schema
/// 里仍合法，只是不参与 chat 多轮上下文恢复（briefing / review / system / highlight
/// 都是不进对话的展示行）。
pub const CHAT_KIND_CHAT: &str = "chat";
pub const CHAT_KIND_COMPACT_BOUNDARY: &str = "compact_boundary";

// ====== Block ⇄ JSON ======================================================

/// 把 `Vec<Block>` 序列化为 `content_json.blocks` 数组。
///
/// 用 serde_json 直接走 [`Block`] 自身的 `#[serde(tag = "type")]` 形态——
/// JSON 里就是 `[{"type":"text","text":...},{"type":"tool_use",...}, ...]`。
pub fn blocks_to_json(blocks: &[Block]) -> Value {
    serde_json::to_value(blocks).unwrap_or(Value::Array(Vec::new()))
}

/// 从 `content_json.blocks` 反序列化回 `Vec<Block>`；失败返回 None。
fn json_to_blocks(v: &Value) -> Option<Vec<Block>> {
    serde_json::from_value::<Vec<Block>>(v.clone()).ok()
}

/// 单条 chat_messages 行 → 结构化 [`Message`]。
///
/// 规则：
/// 1. 若 `content_json.blocks` 是合法 Block 数组 → 直接用
/// 2. 否则用 `content_md` 拼一条 `Block::Text`（兼容老库）
/// 3. role 不在 user/assistant 里 → 返回 None（system/compact_boundary 类行另行处理）
pub fn row_to_message(row: &Value) -> Option<Message> {
    let role_str = row.get("role").and_then(Value::as_str)?;
    let role = match role_str {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };

    // 优先解结构化 blocks
    let content_json = row.get("contentJson");
    if let Some(blocks_v) = content_json.and_then(|c| c.get("blocks")) {
        if let Some(blocks) = json_to_blocks(blocks_v) {
            if !blocks.is_empty() {
                return Some(Message {
                    role,
                    content: blocks,
                });
            }
        }
    }

    // 兜底：contentMd → 单条 Text block
    let text = row.get("contentMd").and_then(Value::as_str).unwrap_or("");
    if text.trim().is_empty() {
        return None;
    }
    Some(Message {
        role,
        content: vec![Block::Text {
            text: text.to_string(),
            cache_control: false,
        }],
    })
}

// ====== compact_boundary 行 ===============================================

/// 抽出一条 compact_boundary 行的摘要文本——`createdAt` 不需要往上传，因为
/// 调用方只关心摘要内容（生成时间已经隐含在"我们刚才在最新 boundary 之后" 这个
/// 语义里）。
fn row_to_boundary_summary(row: &Value) -> Option<String> {
    let kind = row.get("kind").and_then(Value::as_str)?;
    if kind != CHAT_KIND_COMPACT_BOUNDARY {
        return None;
    }
    let summary_text = row
        .get("contentMd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if summary_text.trim().is_empty() {
        return None;
    }
    Some(summary_text)
}

// ====== thread 加载入口 ===================================================

/// 加载结构化 chat 历史——返回 `(messages, boundary_summary?)`。
///
/// - **若存在 compact_boundary 行**：只读 boundary 之后的 user/assistant 消息（结构化），
///   boundary 摘要单独返回，由调用方决定怎么注入（一般作为 messages[0] 的 user 摘要 block）。
/// - **若不存在**：返回所有真实对话消息（结构化），boundary_summary = None。
///
/// 列表按时间正序（旧 → 新），方便直接 append 当前 user 提问到末尾。
///
/// **没有条数截断**——主流共识（Cursor / Cline / Roo / Aider / OpenAI Agents SDK /
/// LangGraph / Anthropic Cookbook）一致：DB 读取层不做语义截断，先读全，由
/// compact tier（MicroClear → Summarize → Drop → HardLimit）按 token 决定保留多少。
/// 单用户 + SQLite + compact_boundary 命中后只读 boundary-after 切片，实际工作集很小。
///
/// `exclude_id`：跳过指定 id 的消息——chat pipeline 在写完本轮 user 消息后立刻读
/// 历史，需要排除"自己"，否则当前提问会出现两次。
pub fn read_recent_chat_thread(
    app: &AppHandle,
    exclude_id: Option<&str>,
) -> (Vec<Message>, Option<String>) {
    // read_all_chat_messages 按 created_at desc——最新在前；无条数截断。
    let raw = crate::infrastructure::agent::repository::read_all_chat_messages(app, None)
        .unwrap_or_default();

    // 排除调用方指定的 id（一般是本轮刚写入的 user message）
    let filtered: Vec<&Value> = raw
        .iter()
        .filter(|r| match exclude_id {
            Some(eid) => r.get("id").and_then(Value::as_str) != Some(eid),
            None => true,
        })
        .collect();

    // 找最新 boundary（desc 顺序里第一条 kind=compact_boundary 即最新）
    let boundary_idx = filtered
        .iter()
        .position(|r| r.get("kind").and_then(Value::as_str) == Some(CHAT_KIND_COMPACT_BOUNDARY));

    let (slice, boundary_summary) = match boundary_idx {
        Some(idx) => {
            let boundary_row = filtered[idx];
            // boundary 之后（更新）= 在 desc 列表里更靠前的部分（idx 之前）
            (&filtered[..idx], row_to_boundary_summary(boundary_row))
        }
        None => {
            // 没有 boundary——全部历史
            (&filtered[..], None)
        }
    };

    // 反序列化 + 反转成正序
    let mut messages: Vec<Message> = slice
        .iter()
        // 跳过非 user/assistant 类行（kind=system / briefing / review / highlight）
        // 这些不是真实对话，不该塞进 messages
        .filter(|r| {
            let kind = r.get("kind").and_then(Value::as_str).unwrap_or("");
            kind == CHAT_KIND_CHAT
        })
        .filter_map(|r| row_to_message(r))
        .collect();
    messages.reverse();
    (messages, boundary_summary)
}

// ====== 写库辅助 ==========================================================

/// 构造 chat_messages user 行的 contentJson——blocks 字段记录结构化内容，
/// images 字段保留用户附带图片路径，前端渲染缩略图。
pub fn build_user_content_json(blocks: &[Block], image_paths: &[String]) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("blocks".into(), blocks_to_json(blocks));
    if !image_paths.is_empty() {
        obj.insert("images".into(), json!(image_paths));
    }
    Value::Object(obj)
}

/// 构造 assistant 行的 contentJson——blocks 字段 + 运行元数据。
/// `extras` 是其他需要并入 contentJson 的字段（runId / turns / localToolCalls 等）。
pub fn build_assistant_content_json(blocks: &[Block], extras: Value) -> Value {
    let mut obj = match extras {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert("blocks".into(), blocks_to_json(blocks));
    Value::Object(obj)
}

/// 构造 compact_boundary 行——只有 kind / contentMd（摘要文本）/ contentJson 里
/// 携带原始边界信息（被压缩的消息 id 范围 / 调用的 summarize 模型 / 原始 token 数）。
///
/// 调用方再补 id / createdAt / 三个 source 字段。
pub fn build_compact_boundary_row(
    summary_text: &str,
    pre_compact_tokens: u32,
    summarize_model: &str,
    dropped_message_count: u32,
) -> Value {
    json!({
        "role": "system",
        "kind": CHAT_KIND_COMPACT_BOUNDARY,
        "contentMd": summary_text,
        "contentJson": {
            "preCompactTokens": pre_compact_tokens,
            "summarizeModel": summarize_model,
            "droppedMessageCount": dropped_message_count,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::types::ToolResultContent;
    use serde_json::json;

    #[test]
    fn row_with_blocks_returns_structured_message() {
        let row = json!({
            "role": "assistant",
            "contentMd": "raw markdown",
            "contentJson": {
                "blocks": [
                    {"type": "text", "text": "hi"},
                    {"type": "tool_use", "id": "toolu_1", "name": "get_quote", "input": {"code": "600519"}}
                ]
            }
        });
        let msg = row_to_message(&row).expect("应该能解出 message");
        assert!(matches!(msg.role, Role::Assistant));
        assert_eq!(msg.content.len(), 2);
        match &msg.content[1] {
            Block::ToolUse { name, .. } => assert_eq!(name, "get_quote"),
            _ => panic!("第二块应该是 tool_use"),
        }
    }

    #[test]
    fn row_without_blocks_falls_back_to_content_md() {
        let row = json!({
            "role": "user",
            "contentMd": "老消息文本",
            "contentJson": null,
        });
        let msg = row_to_message(&row).unwrap();
        assert_eq!(msg.content.len(), 1);
        match &msg.content[0] {
            Block::Text { text, .. } => assert_eq!(text, "老消息文本"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn empty_content_md_and_no_blocks_returns_none() {
        let row = json!({
            "role": "user",
            "contentMd": "",
            "contentJson": null,
        });
        assert!(row_to_message(&row).is_none());
    }

    #[test]
    fn system_role_returns_none() {
        let row = json!({
            "role": "system",
            "contentMd": "对话失败：xxx",
            "contentJson": null,
        });
        assert!(row_to_message(&row).is_none());
    }

    #[test]
    fn build_user_content_json_omits_images_when_empty() {
        let blocks = vec![Block::Text {
            text: "hi".into(),
            cache_control: false,
        }];
        let v = build_user_content_json(&blocks, &[]);
        assert!(v.get("blocks").is_some());
        assert!(v.get("images").is_none());
    }

    #[test]
    fn tool_result_block_round_trips() {
        let original = vec![Block::ToolResult {
            tool_use_id: "toolu_42".into(),
            content: vec![ToolResultContent::Text {
                text: "{\"ok\":true}".into(),
            }],
            is_error: false,
            server_side: false,
            cache_control: false,
        }];
        let json_v = blocks_to_json(&original);
        let row = json!({
            "role": "user",
            "contentMd": "",
            "contentJson": { "blocks": json_v }
        });
        let msg = row_to_message(&row).unwrap();
        match &msg.content[0] {
            Block::ToolResult { tool_use_id, .. } => assert_eq!(tool_use_id, "toolu_42"),
            _ => panic!("expected tool_result"),
        }
    }
}
