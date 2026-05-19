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

use crate::domain::agent::types::{Block, Message, Role, ToolResultContent};
use serde_json::{json, Value};
use tauri::AppHandle;

/// History 加载时把 ToolResult 内容替换成的 stub 文本。
///
/// 30 token 占位文本——告诉 agent"这是过期数据，要新结果重新调用"。保留
/// ToolResult 结构（id / is_error）保证 tool_use ↔ tool_result 配对完整。
const HISTORY_TOOL_RESULT_STUB: &str =
    "[历史工具结果已清理 — 数据过期，需要新数据请重新调用对应工具]";

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

/// 加载结构化 chat 历史——返回 [`ChatThreadLoad`]。
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
///
/// `last_assistant_at_ms`：boundary 后最近一条 assistant 消息的 createdAt（ms epoch）；
/// chat.rs 用它配 `time_based_micro_clear` 在 prompt cache 必然失效时预清易腐工具结果。
pub struct ChatThreadLoad {
    pub messages: Vec<Message>,
    pub boundary_summary: Option<String>,
    pub last_assistant_at_ms: Option<i64>,
}

pub fn read_recent_chat_thread(app: &AppHandle, exclude_id: Option<&str>) -> ChatThreadLoad {
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

    // boundary 后最近一条 assistant 消息的时间戳——desc 顺序里第一条 role=assistant。
    // 给 time_based_micro_clear 配合用（隔几小时再问时清易腐工具白名单结果）。
    let last_assistant_at_ms = slice.iter().find_map(|r| {
        if r.get("kind").and_then(Value::as_str) != Some(CHAT_KIND_CHAT) {
            return None;
        }
        if r.get("role").and_then(Value::as_str) != Some("assistant") {
            return None;
        }
        parse_created_at_ms(r)
    });

    // 不开时间窗——boundary 之后的真实对话全加载。strip_volatile_blocks 把
    // ToolResult 内容换 stub（30 token/个），单条消息已经够小；进入 run_agent 后
    // compact_if_needed 三级压缩 + reactive 兜底接管极端情况。
    let mut messages: Vec<Message> = slice
        .iter()
        // 跳过非 user/assistant 类行（kind=system / briefing / review / highlight）
        .filter(|r| {
            let kind = r.get("kind").and_then(Value::as_str).unwrap_or("");
            kind == CHAT_KIND_CHAT
        })
        .filter_map(|r| row_to_message(r))
        .filter_map(strip_volatile_blocks)
        .collect();
    messages.reverse();
    ChatThreadLoad {
        messages,
        boundary_summary,
        last_assistant_at_ms,
    }
}

/// 历史消息的内容裁剪：
///
/// - **Text / Image**：保留——对话语境 + 用户提问
/// - **ToolUse**：**保留**——决策路径记录（"agent 当时调过哪些工具"）。token 占用极小
///   （~50/个），但让 agent 能感知自己做过什么动作，避免重复调
/// - **ToolResult**：**结构保留 + 内容换 stub**——数据已过期，但配对必须留全
///   （tool_use ↔ tool_result 缺一会 protocol 拒），stub 文本告诉 agent
///   "要新数据请重调"
/// - **Thinking / RedactedThinking**：删——agent 旧推理留着会 anchor 当前判断
///   （§ 2 Bull/Bear Steelman 要治的偏差），让 agent 每轮重新审视
///
/// 返回 None 表示裁剪后空消息（整条丢，避免 provider 拒收空 content）。
fn strip_volatile_blocks(msg: Message) -> Option<Message> {
    let kept: Vec<Block> = msg
        .content
        .into_iter()
        .filter_map(|b| match b {
            Block::Text { .. } | Block::Image { .. } | Block::ToolUse { .. } => Some(b),
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                server_side,
                cache_control,
            } => {
                // 已经是 stub（来自 MicroClear 或之前的 strip）→ 原样保留，幂等
                let already_stub = content.len() == 1
                    && matches!(content.first(), Some(ToolResultContent::Text { text })
                        if text.starts_with("[历史工具结果已清理")
                            || text.starts_with("[过期工具结果已清理"));
                let new_content = if already_stub {
                    content
                } else {
                    vec![ToolResultContent::Text {
                        text: HISTORY_TOOL_RESULT_STUB.into(),
                    }]
                };
                Some(Block::ToolResult {
                    tool_use_id,
                    content: new_content,
                    is_error,
                    server_side,
                    cache_control,
                })
            }
            Block::Thinking { .. } | Block::RedactedThinking { .. } => None,
        })
        .collect();
    if kept.is_empty() {
        return None;
    }
    Some(Message {
        role: msg.role,
        content: kept,
    })
}

/// chat_messages 行的 createdAt 字段有两种形态：ISO8601 字符串、ms epoch 数字。
/// 这里两种都接，统一返 ms epoch。
fn parse_created_at_ms(row: &Value) -> Option<i64> {
    let v = row.get("createdAt")?;
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    if let Some(s) = v.as_str() {
        return chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.timestamp_millis());
    }
    None
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
    fn strip_volatile_keeps_text_image_tool_use_drops_thinking() {
        // 完整的 assistant 消息：text + thinking + tool_use
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                Block::Text {
                    text: "茅台 ¥1520，趋势上行".into(),
                    cache_control: false,
                },
                Block::Thinking {
                    thinking: "我先看 K 线再做判断".into(),
                    signature: None,
                },
                Block::ToolUse {
                    id: "toolu_1".into(),
                    name: "get_kline".into(),
                    input: json!({"code": "600519"}),
                    server_side: false,
                },
            ],
        };
        let stripped = strip_volatile_blocks(msg).expect("应该有内容残留");
        // 保留 text + tool_use；删 thinking
        assert_eq!(stripped.content.len(), 2);
        let has_text = stripped.content.iter().any(|b| matches!(b, Block::Text { text, .. } if text == "茅台 ¥1520，趋势上行"));
        let has_tool_use = stripped.content.iter().any(|b| matches!(b, Block::ToolUse { name, .. } if name == "get_kline"));
        let has_thinking = stripped.content.iter().any(|b| matches!(b, Block::Thinking { .. }));
        assert!(has_text, "text 应保留");
        assert!(has_tool_use, "tool_use 应保留（决策路径）");
        assert!(!has_thinking, "thinking 应删（旧推理 anchor）");
    }

    #[test]
    fn strip_volatile_replaces_tool_result_with_stub() {
        // 一条 user 消息只有 tool_result（大块原始数据）——结构保留，内容换 stub
        let msg = Message {
            role: Role::User,
            content: vec![Block::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: vec![ToolResultContent::Text {
                    text: "{\"code\":\"600519\",\"price\":1520.0, ...10k token JSON...}".into(),
                }],
                is_error: false,
                server_side: false,
                cache_control: false,
            }],
        };
        let stripped = strip_volatile_blocks(msg).expect("整条不应被丢——结构保留");
        match &stripped.content[0] {
            Block::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                assert_eq!(tool_use_id, "toolu_1", "id 必须保留以维持配对");
                match &content[0] {
                    ToolResultContent::Text { text } => {
                        assert!(
                            text.starts_with("[历史工具结果已清理"),
                            "内容应被替换成 stub，实际：{text:.60}"
                        );
                    }
                    _ => panic!("应为 text stub"),
                }
            }
            _ => panic!("应仍是 tool_result"),
        }
    }

    #[test]
    fn strip_volatile_idempotent_on_existing_stubs() {
        // 已经是 stub 的 tool_result 再 strip 一次不应套层
        let msg = Message {
            role: Role::User,
            content: vec![Block::ToolResult {
                tool_use_id: "toolu_a".into(),
                content: vec![ToolResultContent::Text {
                    text: "[过期工具结果已清理 — 需要最新数据请重新调用 `get_quote`]".into(),
                }],
                is_error: false,
                server_side: false,
                cache_control: false,
            }],
        };
        let stripped = strip_volatile_blocks(msg).expect("应保留");
        if let Block::ToolResult { content, .. } = &stripped.content[0] {
            if let ToolResultContent::Text { text } = &content[0] {
                // 不该出现"[历史工具结果已清理][过期工具结果已清理"嵌套
                assert!(
                    !text.contains("[历史工具结果已清理"),
                    "已 stub 应原样保留，不套新 stub: {text}"
                );
            }
        }
    }

    #[test]
    fn strip_volatile_returns_none_for_thinking_only_msg() {
        // 只有 thinking 的消息（罕见）——strip 后空，整条丢
        let msg = Message {
            role: Role::Assistant,
            content: vec![Block::Thinking {
                thinking: "只是想想".into(),
                signature: None,
            }],
        };
        assert!(strip_volatile_blocks(msg).is_none());
    }

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
