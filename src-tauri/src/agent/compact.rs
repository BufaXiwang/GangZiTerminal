//! Summarize tier——用便宜模型把老对话压成一段中文摘要 + 边界 user 消息。
//!
//! 设计取自 claude code 的 compactConversation 思路，但**针对投资 chat 改写了
//! 整个 prompt + 输出协议**：6 段（关注标的 / 已建立判断 / 未决问题 / 风险纪律 /
//! 用户偏好 / 上一句聊什么），强制中文输出，强制 no-tools。
//!
//! 调用方（loop）的语义：
//! 1. MicroClear 跑完后，若 tokens 还 > summarize_threshold → 进 Summarize
//! 2. Summarize 把 messages[..keep_from] 压成一段 boundary 文本，
//!    替换为单条 user message（cache_control: true，作新前缀缓存基底），
//!    后接尾部 keep_from 之外的真实消息
//! 3. 返回的 [`SummarizeOutcome`] 同时给 loop 和 pipeline 用——
//!    loop 用 `messages` 继续这一轮的 provider 调用；
//!    pipeline 用 `boundary_summary_text` 落 chat_messages 的 compact_boundary 行
//!    （让下次 chat 也能继承这条摘要）
//!
//! 失败模型：
//! - provider 报错（429/transient）：调用方应记账失败次数，多次失败后熔断
//! - 模型输出里没找到 `<summary>` 标签：返回 `Err(ParseFailed)`
//! - 模型偷调工具：当前 prompt 没声明任何 tool，provider 不允许调；
//!   极端情况下返回的 stop_reason 不是 EndTurn 时按 ParseFailed 处理

use crate::agent::provider::{ChatProvider, ProviderError};
use crate::agent::types::{
    AgentOptions, AgentRequest, Block, ContextBudget, Message, PipelineKind, Role, StopReason,
    SystemBlock, ToolDef,
};
use futures_util::StreamExt;
use std::sync::Arc;
use thiserror::Error;

/// Summarize 一次的成果——loop 拿到这个就把 messages 整段替换。
#[derive(Debug, Clone)]
pub struct SummarizeOutcome {
    /// 压缩后的完整 messages 列表：[boundary_user, kept_msg_1, kept_msg_2, ...]
    pub messages: Vec<Message>,
    /// 边界摘要文本——pipeline 落 compact_boundary chat_messages 行用。
    pub boundary_summary_text: String,
    /// 这次摘要 API 调用的 input/output token 用量（做观测）。
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// 摘要替换掉的原始消息条数（用于 emit Compacted 事件）。
    pub dropped_messages: u32,
}

#[derive(Debug, Error)]
pub enum SummarizeError {
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    /// 模型输出里没有合法 `<summary>...</summary>` 段——可能被工具调用 / 截断 /
    /// 偏离指令。
    #[error("无法从模型回复中解析摘要：{0}")]
    ParseFailed(String),
}

/// 摘要 prompt 的 system 段——投资学习 chat 专用。
///
/// 输出协议：
/// - `<analysis>` 块：草稿，formatCompactSummary 会去掉，不会进入续档
/// - `<summary>` 块：6 段，作续档主体
const SUMMARIZE_SYSTEM: &str = r#"你正在为一段投资学习对话生成续档摘要。该摘要会替换早期对话，让另一个 agent 接着往下聊而不丢上下文。

**严禁调用任何工具**——你看到的对话已经是全部，调工具会被拒绝并浪费这次唯一回合。
请用 Markdown 输出纯文本，先用 <analysis> 块打草稿（不会进入续档），再用 <summary> 块输出 6 段：

1. 关注标的：用户在这段对话里提到 / 查过的股票或板块。每个标的标注用户立场（持有 / 想买 / 观察 / 排除）。格式建议：`600519 贵州茅台 — 观察，担心估值`。
2. 已建立的判断：本段已得出的关键结论 + 依据，包括用过的数据点（哪只股价、哪条新闻、哪个 KPI）和判断逻辑。**用户的反对意见也要保留**——他纠正过的思路比赞同过的更重要。
3. 未决问题：本段抛出但没回答的问题、未验证的假设、用户说"先放一放"的话题。
4. 风险 / 纪律决定：止损线、仓位上限、回撤容忍度等用户当场确认或调整过的规则。
5. 用户偏好与禁忌：本段暴露的长期偏好（例如"我不碰白酒"、"只做超短线"）。
6. 上一句在聊什么：用户最后一条消息的字面引述 + 当时还没收尾的具体动作。下一个 agent 看到这段就能无缝续聊。

注意：
- 不要逐条复述工具结果——价格、K 线已经过期，复述等于喂老数据。
- 用户的原话引述要精确，不要"用户大概意思是…"。
- 整个 <summary> 控制在 1500 token 内。
- 全程使用中文。
"#;

/// 给摘要器看的 user 提示——尾部，提醒"现在请输出"。
const SUMMARIZE_USER_TRAILER: &str =
    "请基于以上对话历史，按 system 中的 6 段结构输出 <analysis>...</analysis><summary>...</summary>。不要调用任何工具，只输出纯文本。";

/// 把对话压成边界摘要 + 尾部保留消息。
///
/// 入参：
/// - `provider`：复用 chat 的 provider 实例（同一 base_url + token，但用便宜模型）
/// - `model`：摘要模型 id（如 `claude-haiku-4-5` / `gpt-5.5-nano`）
/// - `messages_before`：完整的当前 messages 列表（旧 → 新）
/// - `keep_last_n_turns`：尾部要原样保留的 user/assistant 对数（一对计 2 条）
/// - `budget`：原 chat run 的 budget——摘要请求自身不需要 compact，借用做透传
///
/// 出参：[`SummarizeOutcome`]，包含替换后的 messages 和原文摘要。
///
/// 内部行为：
/// - 选出 messages[..keep_from] 作为"待摘要段"
/// - 构造一个 no-tools AgentRequest，messages 形态：
///   1. user 文本："以下是历史对话原文，按时间正序"
///   2. user 块：把 messages_to_compress 序列化成 `<turn role="user">...` 形态拼一段长文本
///   3. user 文本：SUMMARIZE_USER_TRAILER
/// - 调用 provider.stream，drain 所有 ProviderEvent，组装 assistant 回复
/// - 解析 `<summary>` 段
/// - 返回 [boundary_user_msg, kept_msgs[keep_from..]]
pub async fn summarize_messages(
    provider: &Arc<dyn ChatProvider>,
    model: &str,
    messages_before: &[Message],
    keep_last_n_turns: u32,
    budget: &ContextBudget,
) -> Result<SummarizeOutcome, SummarizeError> {
    let keep_n = (keep_last_n_turns as usize).max(1);
    let keep_from = messages_before
        .len()
        .saturating_sub(keep_n.saturating_mul(2));
    if keep_from == 0 {
        // 没有可压缩的尾窗外消息——直接返回原 messages，不调模型
        return Ok(SummarizeOutcome {
            messages: messages_before.to_vec(),
            boundary_summary_text: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            dropped_messages: 0,
        });
    }
    let to_compress = &messages_before[..keep_from];
    let to_keep = &messages_before[keep_from..];

    let serialized_history = serialize_history_for_prompt(to_compress);

    let summarize_req = AgentRequest {
        system: vec![SystemBlock {
            text: SUMMARIZE_SYSTEM.to_string(),
            cache_control: false,
        }],
        // 不带任何工具——任何 tool_call 都会让流程崩
        tools: Vec::<ToolDef>::new(),
        messages: vec![Message {
            role: Role::User,
            content: vec![Block::Text {
                text: format!(
                    "以下是早期对话原文（按时间正序）：\n\n{serialized_history}\n\n{SUMMARIZE_USER_TRAILER}"
                ),
                cache_control: false,
            }],
        }],
        options: AgentOptions {
            model: model.to_string(),
            // 1500 token 摘要 + 草稿空间——给 3000 token 一般够。
            max_tokens: 3000,
            // 摘要不需要发散——用低温度求稳。
            temperature: Some(0.2),
            top_p: None,
            thinking: None,
            // 摘要任务用 low effort——是 mechanical 工作不需要深度推理，省 token + 加速
            effort: Some(crate::agent::types::EffortLevel::Low),
            // 摘要器不调工具，max_turns=1 即可（实际上 provider 一次响应就 EndTurn）
            max_turns: 1,
            stop_sequences: vec![],
            tool_timeout_secs: None,
        },
        budget: budget.clone(),
        trigger_message_id: None,
        pipeline: PipelineKind::Chat,
    };

    let mut stream = provider.stream(&summarize_req).await?;
    let mut text = String::new();
    let mut input_tokens = 0u32;
    let mut output_tokens = 0u32;
    let mut stop_reason: Option<StopReason> = None;

    while let Some(ev) = stream.next().await {
        match ev? {
            crate::agent::provider::ProviderEvent::TextDelta(d) => text.push_str(&d),
            crate::agent::provider::ProviderEvent::Usage(u) => {
                input_tokens += u.input_tokens;
                output_tokens += u.output_tokens;
            }
            crate::agent::provider::ProviderEvent::MessageComplete {
                message,
                stop_reason: sr,
            } => {
                // 兜底：如果 TextDelta 流没拼出来（极少见），从 MessageComplete 里取
                if text.is_empty() {
                    for block in message.content {
                        if let Block::Text { text: t, .. } = block {
                            text.push_str(&t);
                        }
                    }
                }
                stop_reason = Some(sr);
            }
            _ => {}
        }
    }

    if !matches!(stop_reason, Some(StopReason::EndTurn)) {
        return Err(SummarizeError::ParseFailed(format!(
            "摘要响应非正常结束：stop_reason={:?}",
            stop_reason
        )));
    }

    let summary = extract_summary_block(&text)
        .ok_or_else(|| SummarizeError::ParseFailed("响应里找不到 <summary>...</summary>".into()))?;
    if summary.trim().is_empty() {
        return Err(SummarizeError::ParseFailed("<summary> 段为空".into()));
    }

    let boundary_text = format!(
        "[历史压缩边界——以下是早期对话的摘要，请视作既成事实]\n\n{summary}\n\n[摘要结束，下面是边界之后的真实对话]"
    );

    let mut new_messages: Vec<Message> = Vec::with_capacity(1 + to_keep.len());
    new_messages.push(Message {
        role: Role::User,
        content: vec![Block::Text {
            text: boundary_text,
            // 摘要后整体作为新 prefix——打 cache_control 让后续 chat turn 复用
            cache_control: true,
        }],
    });
    new_messages.extend_from_slice(to_keep);

    Ok(SummarizeOutcome {
        messages: new_messages,
        boundary_summary_text: summary.trim().to_string(),
        input_tokens,
        output_tokens,
        dropped_messages: keep_from as u32,
    })
}

/// 把历史 messages 序列化成可读文本——给摘要模型阅读，不再保留原始结构。
fn serialize_history_for_prompt(messages: &[Message]) -> String {
    let mut out = String::new();
    for (i, msg) in messages.iter().enumerate() {
        let role = match msg.role {
            Role::User => "用户",
            Role::Assistant => "Agent",
        };
        out.push_str(&format!("<turn idx=\"{i}\" role=\"{role}\">\n"));
        for block in &msg.content {
            match block {
                Block::Text { text, .. } => out.push_str(text),
                Block::Thinking { thinking, .. } => {
                    out.push_str("[thinking] ");
                    out.push_str(thinking);
                }
                Block::RedactedThinking { .. } => out.push_str("[redacted thinking]"),
                Block::Image { .. } => out.push_str("[图片附件]"),
                Block::ToolUse { name, input, .. } => {
                    out.push_str(&format!("[调工具 {name}({})]", input.to_string()));
                }
                Block::ToolResult {
                    content, is_error, ..
                } => {
                    if *is_error {
                        out.push_str("[工具失败：");
                    } else {
                        out.push_str("[工具结果：");
                    }
                    for c in content {
                        match c {
                            crate::agent::types::ToolResultContent::Text { text } => {
                                out.push_str(text);
                            }
                            crate::agent::types::ToolResultContent::Image { .. } => {
                                out.push_str("[image]");
                            }
                            crate::agent::types::ToolResultContent::Json { raw } => {
                                out.push_str(&raw.to_string());
                            }
                        }
                    }
                    out.push(']');
                }
            }
            out.push('\n');
        }
        out.push_str("</turn>\n\n");
    }
    out
}

/// 抽取 `<summary>...</summary>` 内容——容忍多行 + 不区分大小写。
fn extract_summary_block(s: &str) -> Option<String> {
    // 先丢掉 <analysis>...</analysis> 草稿
    let cleaned = strip_analysis_block(s);
    let lower = cleaned.to_lowercase();
    let open = lower.find("<summary>")?;
    let after_open = open + "<summary>".len();
    let close_relative = lower[after_open..].find("</summary>")?;
    Some(cleaned[after_open..after_open + close_relative].to_string())
}

fn strip_analysis_block(s: &str) -> String {
    let lower = s.to_lowercase();
    let open = match lower.find("<analysis>") {
        Some(i) => i,
        None => return s.to_string(),
    };
    let after_open = open + "<analysis>".len();
    let close_relative = match lower[after_open..].find("</analysis>") {
        Some(i) => i,
        None => return s.to_string(),
    };
    let close_end = after_open + close_relative + "</analysis>".len();
    let mut out = String::new();
    out.push_str(&s[..open]);
    out.push_str(&s[close_end..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_summary_handles_well_formed() {
        let s = r#"<analysis>think think</analysis>
<summary>
1. 关注标的：600519 贵州茅台 — 观察
</summary>"#;
        let got = extract_summary_block(s).unwrap();
        assert!(got.contains("贵州茅台"));
        assert!(!got.contains("think think"), "analysis 段不该出现");
    }

    #[test]
    fn extract_summary_returns_none_when_missing() {
        let s = "纯文本，没有标签";
        assert!(extract_summary_block(s).is_none());
    }

    #[test]
    fn strip_analysis_when_no_summary_tag() {
        // 没有 <summary> 但有 <analysis>——extract 应返回 None
        let s = "<analysis>just thinking</analysis>";
        assert!(extract_summary_block(s).is_none());
    }
}
