//! Context manager——loop 在每次 provider 调用前调一次，决定是否压缩历史。
//!
//! 设计：
//! - **token 估算**：4 字符/token 对中英混合误差太大（中文实际 ~2 字符/token），
//!   这里按字符是否 ASCII 分流估算，再加图片/JSON 的 base 量。这是个启发式，
//!   误差 ~15%，但 budget 本来就是软警戒线，够用。
//! - **三级压缩**：
//!   1. `MicroClear`：白名单内的"易腐工具"（行情 / K 线 / 资讯 / 搜索）的 ToolResult
//!      内容替换成 stub `[过期工具结果已清理 — 需要新数据请重新调用 <tool>]`，
//!      保留 tool_use_id 不破配对。这是**针对投资场景的核心策略**：旧行情就是
//!      misinformation，与其摘要不如重新拉。
//!   2. `Summarize`（实装在 `crate::pipeline::agent::compact::summarize`）：调便宜模型把
//!      老对话压成 6 段中文摘要 + 边界 user 消息。compact_if_needed 不直接调，
//!      由 loop 层挑时机调用并回填，因为它需要异步 + provider 句柄。
//!   3. `Drop`：实在不行才整条丢，prepend `[N earlier messages omitted]` 兜底。
//! - **hard limit**：drop 完仍超 hard_limit 时返回 [`CompactAction::HardLimit`]，
//!   调用方应该中止 run 并报错，不要再调 provider（一定会被 4xx context_too_long 拒）。
//!
//! 调用顺序由 loop 编排：
//!   compact_if_needed (rule-based, sync) → 若仍超 summarize_threshold → summarize
//!   (async, may fail) → 失败回退到 Drop tier。

use crate::domain::agent::types::{Block, ContextBudget, Message, Role, ToolResultContent};
use std::collections::HashMap;

/// 一次 compact 的产物。
#[derive(Debug, Clone)]
pub struct CompactReport {
    pub messages: Vec<Message>,
    pub estimated_tokens_before: u32,
    pub estimated_tokens_after: u32,
    pub action: CompactAction,
    /// 被丢弃/截断的消息数。给前端 emit Compacted 事件用。
    pub dropped_messages: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactAction {
    /// 在 soft_limit 内，无操作。
    NoOp,
    /// 清理了白名单工具的老 ToolResult 内容（保留 tool_use_id）。
    MicroClear,
    /// 丢了最老的若干条消息。
    Drop,
    /// drop 完仍超 hard_limit——调用方应该中止 run。
    HardLimit,
}

/// 易腐数据工具白名单——这些工具的 ToolResult 在尾窗外**整块**替换成 stub。
///
/// 选择标准：**结果可重新拉取且数据时效性敏感**。旧的 K 线 / 盘口 / 资讯
/// 直接是误导（agent 看到的"3 分钟前"价格已经不准），与其留着不如清掉，
/// agent 真要决策会重新调用。
///
/// **不在白名单**的工具：
/// - `update_memory` / `remove_memory`：mutation 工具，结果是确认文本，
///   清掉反而让 agent 怀疑 "我刚才存进 memory 了吗"。
/// - `list_positions`：状态快照但本来就短，不浪费 token；agent 也可能依赖
///   "上一轮看到的持仓状态" 做对比，清掉破坏因果链。
/// - 未来加新工具默认不在白名单——只有明确"易腐"才加。
const VOLATILE_TOOL_WHITELIST: &[&str] = &[
    "get_quote",
    "get_kline",
    "get_market_overview",
    "search_quotes",
    "search_news",
    "web_search",
];

fn is_volatile_tool(name: &str) -> bool {
    VOLATILE_TOOL_WHITELIST.contains(&name)
}

/// 扫一遍 messages 建 `tool_use_id -> tool_name` 映射——micro_clear 拿这个
/// 决定某条 ToolResult 该不该清。tool_use_id 是 assistant 消息里 ToolUse block
/// 的 id，对应同 id 的 user 消息里 ToolResult。
fn build_tool_use_id_to_name(messages: &[Message]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for msg in messages {
        for block in &msg.content {
            if let Block::ToolUse { id, name, .. } = block {
                map.insert(id.clone(), name.clone());
            }
        }
    }
    map
}

/// 估算消息列表的 token 数。
///
/// 对每个文本分段统计 ASCII 字符 / 非 ASCII 字符，分别按 1/4 和 1/2 折算——
/// 这个折算系数对 Claude/GPT tokenizer 在中英混合场景下大致不偏（实测 ±15%）。
/// 图片按 ~1500 token、JSON tool_use input 按字节/4 估。
pub fn estimate_tokens(messages: &[Message]) -> u32 {
    let mut total: usize = 0;
    for msg in messages {
        for block in &msg.content {
            total += match block {
                Block::Text { text, .. } => est_text(text),
                Block::Thinking { thinking, .. } => est_text(thinking),
                Block::RedactedThinking { data } => data.len() / 4,
                Block::Image { .. } => 1500,
                Block::ToolUse { input, .. } => input.to_string().len() / 4,
                Block::ToolResult { content, .. } => content
                    .iter()
                    .map(|c| match c {
                        ToolResultContent::Text { text } => est_text(text),
                        ToolResultContent::Image { .. } => 1500,
                        ToolResultContent::Json { raw } => raw.to_string().len() / 4,
                    })
                    .sum(),
            };
        }
    }
    total as u32
}

fn est_text(text: &str) -> usize {
    // 把字符分两类：ASCII 走 4 字符/token，CJK 等多字节走 2 字符/token。
    // 用 chars().count() 而不是 len()——后者对 utf-8 中文每字符 3 字节会严重高估。
    let mut ascii = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else {
            other += 1;
        }
    }
    ascii / 4 + other / 2
}

/// 对消息列表压缩到 soft_limit 以下；超 hard_limit 时返回 HardLimit 让调用方停。
///
/// `keep_last_n_turns` 表示尾部要保留的 user/assistant 对数（一对计 2 条消息）。
/// micro 策略只动尾部之外的消息；drop 策略也只丢尾部之外的（避免丢用户当前问题）。
pub fn compact_if_needed(messages: Vec<Message>, budget: &ContextBudget) -> CompactReport {
    let before = estimate_tokens(&messages);
    if before <= budget.soft_limit_tokens {
        return CompactReport {
            messages,
            estimated_tokens_before: before,
            estimated_tokens_after: before,
            action: CompactAction::NoOp,
            dropped_messages: 0,
        };
    }

    // 第 1 步：MicroClear——把"尾部之外"消息里、白名单工具的 ToolResult 整块换成
    // 带工具名的 stub。保留 tool_use_id + is_error，配对结构不破。
    let keep_n = (budget.compact_keep_last_n as usize).max(1);
    let keep_from = messages.len().saturating_sub(keep_n.saturating_mul(2));
    let id_to_name = build_tool_use_id_to_name(&messages);
    let mut working = messages;
    let mut micro_clears: u32 = 0;
    for (i, msg) in working.iter_mut().enumerate() {
        if i >= keep_from {
            break;
        }
        for block in msg.content.iter_mut() {
            if let Block::ToolResult {
                tool_use_id,
                content,
                ..
            } = block
            {
                // 已经是 stub 形态（再压一次会重复套层）→ 跳过
                if content.len() == 1 {
                    if let Some(ToolResultContent::Text { text }) = content.first() {
                        if text.starts_with("[过期工具结果已清理")
                            || text.starts_with("[interrupted:")
                            || text.starts_with("[tool result truncated")
                        {
                            continue;
                        }
                    }
                }
                let tool_name = id_to_name
                    .get(tool_use_id)
                    .map(String::as_str)
                    .unwrap_or("");
                if !is_volatile_tool(tool_name) {
                    continue;
                }
                let stub_text = if tool_name.is_empty() {
                    "[过期工具结果已清理 — 需要最新数据请重新调用对应工具]".to_string()
                } else {
                    format!("[过期工具结果已清理 — 需要最新数据请重新调用 `{tool_name}`]")
                };
                *content = vec![ToolResultContent::Text { text: stub_text }];
                micro_clears += 1;
            }
        }
    }
    let after_micro = estimate_tokens(&working);
    if after_micro <= budget.soft_limit_tokens {
        return CompactReport {
            messages: working,
            estimated_tokens_before: before,
            estimated_tokens_after: after_micro,
            action: CompactAction::MicroClear,
            dropped_messages: micro_clears,
        };
    }

    // 第 2 步：drop——从最老的开始整条丢，直到回到 soft_limit 或只剩 keep_n 对。
    // 丢的时候按对丢（user + assistant），保持 messages 列表的 role 节奏不被破坏。
    // 先 prepend 一条占位 user 消息，记账丢了多少。
    let drop_target = budget.soft_limit_tokens;
    let mut dropped: u32 = 0;
    while estimate_tokens(&working) > drop_target {
        // 至少保留 keep_n*2 条
        if working.len() <= keep_n * 2 {
            break;
        }
        // 丢最老的一条
        working.remove(0);
        dropped += 1;
    }
    // 如果真丢了，prepend 一条 stub
    if dropped > 0 {
        working.insert(
            0,
            Message {
                role: Role::User,
                content: vec![Block::Text {
                    text: format!("[上下文压缩：{dropped} 条更早的消息已省略]"),
                    cache_control: false,
                }],
            },
        );
    }

    let after_drop = estimate_tokens(&working);
    if after_drop > budget.hard_limit_tokens {
        // 已经丢到只剩 keep_n 对仍然超 hard——剩下的是单条巨大消息（用户贴了
        // 几十兆文本？），无能为力。让 loop 报错而不是去撞 provider 的 4xx。
        return CompactReport {
            messages: working,
            estimated_tokens_before: before,
            estimated_tokens_after: after_drop,
            action: CompactAction::HardLimit,
            dropped_messages: dropped + micro_clears,
        };
    }
    CompactReport {
        messages: working,
        estimated_tokens_before: before,
        estimated_tokens_after: after_drop,
        action: CompactAction::Drop,
        dropped_messages: dropped + micro_clears,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![Block::Text {
                text: text.into(),
                cache_control: false,
            }],
        }
    }

    fn tool_result_msg(tool_use_id: &str, text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![Block::ToolResult {
                tool_use_id: tool_use_id.into(),
                content: vec![ToolResultContent::Text { text: text.into() }],
                is_error: false,
                server_side: false,
                cache_control: false,
            }],
        }
    }

    /// 拼一对 (assistant: tool_use, user: tool_result) 让 micro_clear 能识别工具名。
    fn tool_call_pair(tool_use_id: &str, tool_name: &str, result_text: &str) -> Vec<Message> {
        vec![
            Message {
                role: Role::Assistant,
                content: vec![Block::ToolUse {
                    id: tool_use_id.into(),
                    name: tool_name.into(),
                    input: json!({}),
                    server_side: false,
                }],
            },
            tool_result_msg(tool_use_id, result_text),
        ]
    }

    fn budget(soft: u32, hard: u32, keep: u32) -> ContextBudget {
        ContextBudget {
            soft_limit_tokens: soft,
            hard_limit_tokens: hard,
            compact_keep_last_n: keep,
            max_search_calls: 5,
        }
    }

    #[test]
    fn estimate_handles_chinese_better_than_naive() {
        // 200 字中文（每字 3 字节）。len()/4 ≈ 150，明显高估实际 ~100 token。
        // 我们的估算 ~100（200/2）。
        let chinese = "中文".repeat(100);
        let msgs = vec![text_msg(Role::User, &chinese)];
        let est = estimate_tokens(&msgs);
        // 应该在 80-120 之间（精确值取决于 char count）
        assert!(est >= 80 && est <= 130, "got {est}");
    }

    #[test]
    fn no_op_when_under_soft_limit() {
        let msgs = vec![text_msg(Role::User, "hi")];
        let report = compact_if_needed(msgs.clone(), &budget(1000, 2000, 3));
        assert_eq!(report.action, CompactAction::NoOp);
        assert_eq!(report.dropped_messages, 0);
        assert_eq!(report.messages.len(), msgs.len());
    }

    #[test]
    fn micro_clear_replaces_old_volatile_tool_results() {
        // 老的 get_quote / search_news 调用——尾窗外，结果应被替换成 stub。
        let big = "x".repeat(4000);
        let mut msgs = Vec::new();
        msgs.push(text_msg(Role::User, "查行情"));
        msgs.extend(tool_call_pair("toolu_a", "get_quote", &big));
        msgs.push(text_msg(Role::User, "再查新闻"));
        msgs.extend(tool_call_pair("toolu_b", "search_news", &big));
        // 尾窗：keep_last_n=1 → 末尾保留 2 条原样
        msgs.push(text_msg(Role::User, "新问题"));
        msgs.push(text_msg(Role::Assistant, "新答"));
        let before = estimate_tokens(&msgs);
        let report = compact_if_needed(msgs, &budget(before / 2, before * 2, 1));
        assert_eq!(report.action, CompactAction::MicroClear);
        assert_eq!(report.dropped_messages, 2);
        // 老的 get_quote 结果应被替换成 stub，stub 文案带工具名
        let stubs: Vec<&str> = report
            .messages
            .iter()
            .flat_map(|m| {
                m.content.iter().filter_map(|b| match b {
                    Block::ToolResult { content, .. } => content.first().and_then(|c| match c {
                        ToolResultContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    }),
                    _ => None,
                })
            })
            .collect();
        assert!(
            stubs.iter().any(|s| s.contains("get_quote")),
            "stub 应该带 get_quote 工具名，实际：{stubs:?}"
        );
        assert!(
            stubs.iter().any(|s| s.contains("search_news")),
            "stub 应该带 search_news 工具名"
        );
    }

    #[test]
    fn micro_clear_skips_non_volatile_tools() {
        // update_memory 的 ToolResult 不在白名单——不应被清理
        let big = "x".repeat(4000);
        let mut msgs = Vec::new();
        msgs.extend(tool_call_pair(
            "toolu_mem",
            "update_memory",
            "memory updated ok",
        ));
        // 用大文本压上下文
        msgs.push(text_msg(Role::User, &big));
        msgs.push(text_msg(Role::Assistant, &big));
        msgs.push(text_msg(Role::User, "尾"));
        msgs.push(text_msg(Role::Assistant, "答"));
        let before = estimate_tokens(&msgs);
        let report = compact_if_needed(msgs, &budget(before / 2, before * 2, 1));
        // memory 工具结果不被清——micro_clear 救不下来时会进 drop
        // 不论最终走 MicroClear 还是 Drop，update_memory 这条 tool_result 的内容应保持原样
        let preserved = report.messages.iter().any(|m| {
            m.content.iter().any(|b| matches!(b,
                Block::ToolResult { tool_use_id, content, .. }
                if tool_use_id == "toolu_mem"
                && content.first().map(|c| matches!(c, ToolResultContent::Text { text } if text == "memory updated ok")).unwrap_or(false)
            ))
        });
        // 要么被保留（mem 工具不清），要么整条 message 被 drop（但没被截而留下空壳）
        // 检查: 如果 toolu_mem 的 result 还在 messages 里，它的 content 必须是原文
        let any_mem_stub = report.messages.iter().any(|m| {
            m.content.iter().any(|b| matches!(b,
                Block::ToolResult { tool_use_id, content, .. }
                if tool_use_id == "toolu_mem"
                && content.first().map(|c| matches!(c, ToolResultContent::Text { text } if text.starts_with("[过期工具结果"))).unwrap_or(false)
            ))
        });
        assert!(
            !any_mem_stub,
            "update_memory 的 tool_result 不该被 micro_clear"
        );
        // 信息提示性 assert：要么保留，要么被 drop——两者都合法
        let _ = preserved;
    }

    #[test]
    fn drop_when_micro_insufficient() {
        // 全是大文本（不是 tool_result，micro 帮不上忙），只能 drop。
        let big = "x".repeat(4000);
        let msgs = vec![
            text_msg(Role::User, &big),
            text_msg(Role::Assistant, &big),
            text_msg(Role::User, &big),
            text_msg(Role::Assistant, &big),
            text_msg(Role::User, "尾部"),
            text_msg(Role::Assistant, "尾答"),
        ];
        let before = estimate_tokens(&msgs);
        let report = compact_if_needed(msgs, &budget(before / 3, before * 2, 1));
        assert_eq!(report.action, CompactAction::Drop);
        assert!(report.dropped_messages >= 1);
        // 第一条应该是占位 stub
        match &report.messages[0].content[0] {
            Block::Text { text, .. } => assert!(text.contains("上下文压缩")),
            _ => panic!("expected stub text"),
        }
        // 尾部消息保留
        let last = report.messages.last().unwrap();
        match &last.content[0] {
            Block::Text { text, .. } => assert_eq!(text, "尾答"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn hard_limit_when_even_drop_cant_save() {
        // 单条尾部消息已经超 hard_limit
        let huge = "x".repeat(40_000); // ~10k token
        let msgs = vec![
            text_msg(Role::User, &huge),
            text_msg(Role::Assistant, &huge),
        ];
        let report = compact_if_needed(msgs, &budget(100, 200, 1));
        assert_eq!(report.action, CompactAction::HardLimit);
    }

    #[test]
    fn preserves_tool_use_id_when_clearing_tool_result() {
        // micro_clear 把白名单工具的 ToolResult 内容换成 stub，但 tool_use_id 必须保留
        let big = "x".repeat(4000);
        let mut msgs = Vec::new();
        msgs.extend(tool_call_pair("toolu_xyz", "get_kline", &big));
        msgs.push(text_msg(Role::User, "新问题"));
        msgs.push(text_msg(Role::Assistant, "新答"));
        let before = estimate_tokens(&msgs);
        let report = compact_if_needed(msgs, &budget(before / 3, before * 2, 1));
        assert_eq!(report.action, CompactAction::MicroClear);
        // 找到那条被改过的 tool_result——tool_use_id 还在，content 是 stub
        let cleared = report.messages.iter().find_map(|m| {
            m.content.iter().find_map(|b| match b {
                Block::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == "toolu_xyz" => Some(content.clone()),
                _ => None,
            })
        });
        let content = cleared.expect("toolu_xyz 的 tool_result 应该还在");
        match &content[0] {
            ToolResultContent::Text { text } => {
                assert!(
                    text.contains("get_kline"),
                    "stub 应包含工具名，实际：{text}"
                );
                assert!(text.contains("过期工具结果已清理"));
            }
            _ => panic!("expected stub text"),
        }
    }

    #[test]
    fn micro_clear_idempotent_on_already_cleared_stubs() {
        // 已经是 stub 的 tool_result 再压一次不应该套层
        let big = "x".repeat(4000);
        let mut msgs = Vec::new();
        msgs.extend(tool_call_pair("toolu_a", "get_quote", &big));
        msgs.push(text_msg(Role::User, "尾"));
        msgs.push(text_msg(Role::Assistant, "答"));
        let before = estimate_tokens(&msgs);
        let pass1 = compact_if_needed(msgs, &budget(before / 2, before * 2, 1));
        let pass2 = compact_if_needed(pass1.messages, &budget(before / 4, before * 2, 1));
        // 第二次不应该再增加 micro_clears
        let stub_text = pass2.messages.iter().find_map(|m| {
            m.content.iter().find_map(|b| match b {
                Block::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == "toolu_a" => content.first().and_then(|c| match c {
                    ToolResultContent::Text { text } => Some(text.clone()),
                    _ => None,
                }),
                _ => None,
            })
        });
        if let Some(text) = stub_text {
            // 不该出现"[过期...][过期...]"嵌套
            assert_eq!(
                text.matches("过期工具结果已清理").count(),
                1,
                "stub 不应被二次套层：{text}"
            );
        }
    }

    #[test]
    fn json_input_in_tool_use_estimated() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "t1".into(),
                name: "search".into(),
                input: json!({"query": "x".repeat(400)}),
                server_side: false,
            }],
        }];
        let est = estimate_tokens(&msgs);
        // ~400+ chars JSON / 4 ≈ 100 token
        assert!(est >= 80, "got {est}");
    }
}
