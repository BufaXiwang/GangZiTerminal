//! Agent loop 流式输出事件。
//!
//! AnthropicProvider 把 SSE 翻译成这套；loop 在工具执行前后追加 ToolStart/ToolEnd；
//! observer 把流转发到 Tauri emit。前端 `useChatMessageStream` 直接消费这套，
//! 不感知 provider 协议差异。

use super::request::PipelineKind;
use super::wire::ToolResultContent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// 一次 run 启动。run_id 从这里开始追踪。
    RunStart {
        run_id: String,
        pipeline: PipelineKind,
        model: String,
    },
    /// 文本增量。前端按 run_id 累积渲染。
    TextDelta { run_id: String, delta: String },
    /// 思考增量（启用 thinking 时）。
    Thinking { run_id: String, delta: String },
    /// 工具调用开始（loop 在调用 ToolRegistry 前 emit）。
    /// server_side=true 时 input 来自 provider，loop 没执行只在转发。
    ToolStart {
        run_id: String,
        tool_use_id: String,
        name: String,
        input: Value,
        server_side: bool,
    },
    /// 工具调用结束。output 是结构化的（文本/图/JSON）。
    ToolEnd {
        run_id: String,
        tool_use_id: String,
        name: String,
        output: Vec<ToolResultContent>,
        is_error: bool,
        duration_ms: u64,
        server_side: bool,
    },
    /// 一次模型调用的 token 用量。每个 turn 都会有一条。
    Usage {
        run_id: String,
        input_tokens: u32,
        output_tokens: u32,
        cache_read_tokens: u32,
        cache_write_tokens: u32,
    },
    /// 上下文压缩了。`tier` 标识哪一档触发的——前端 chip 可以区别渲染：
    /// - `micro_clear`：清掉了易腐工具的旧 ToolResult（无模型调用）
    /// - `summarize`：调便宜模型把老对话摘要成一段中文（有模型调用）
    /// - `drop`：直接丢弃最老消息（兜底）
    /// `dropped_messages` 是被影响的消息条数，`summary_tokens` 是估算节省的 token 数。
    Compacted {
        run_id: String,
        tier: CompactTier,
        dropped_messages: u32,
        summary_tokens: u32,
    },
    /// run 正常结束。turns 是发生了多少次 tool-use 迭代。
    Done {
        run_id: String,
        stop_reason: StopReason,
        turns: u32,
    },
    /// run 异常结束。
    Error { run_id: String, message: String },
}

/// 上下文压缩的档位——`AgentEvent::Compacted.tier` 用。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompactTier {
    /// 清白名单工具的老 ToolResult，纯规则化，无模型调用。
    MicroClear,
    /// 调便宜模型把老对话压成一段中文摘要 + 边界 user 消息。
    Summarize,
    /// 直接丢弃最老消息——proactive 兜底。
    Drop,
    /// Reactive 兜底——provider 返 prompt_too_long 后丢最老 API round 重试。
    /// 触发说明 token 估算 + proactive compact 仍不够；只在 provider 真实拒绝时跑。
    Reactive,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// 模型主动 end_turn——文本回复完成，无 tool_use。
    EndTurn,
    /// 命中 max_tokens 截断。
    MaxTokens,
    /// 命中 stop_sequence。
    StopSequence,
    /// 命中 loop 的 max_turns 硬上限。
    MaxTurns,
    /// 命中 ContextBudget.max_search_calls。
    SearchBudgetExhausted,
    /// 模型拒绝输出（refusal stop reason）。
    Refusal,
    /// pause_turn——长任务暂停，下一轮继续。loop 内部处理，一般不外溢。
    PauseTurn,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_event_run_start_tags_correctly() {
        let ev = AgentEvent::RunStart {
            run_id: "r1".into(),
            pipeline: PipelineKind::Chat,
            model: "claude-sonnet-4-6".into(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "run_start");
        assert_eq!(v["pipeline"], "chat");
    }
}
