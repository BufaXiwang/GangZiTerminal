//! `compact_now` tool —— spec `agent-infra-module.md §4`。
//!
//! Agent 主动释放上下文：调用后立刻返回 ack，loop 在外层观察到本 ToolUse 名字
//! 后会把 `force_summarize_next_turn` 置 true，下一轮强制跑 Summarize。
//!
//! 工具本身是纯信号（no side effect），让 loop 知道"该 compact 了"。

use crate::domain::agent::types::ToolResultContent;
use crate::pipeline::agent::tools::{ok_json, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::{json, Value};

// CompactNowTool 没有副作用（信号工具）；side_effect() 走默认 SideEffect::None。

pub struct CompactNowTool;

impl CompactNowTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for CompactNowTool {
    fn name(&self) -> &'static str {
        "compact_now"
    }

    fn description(&self) -> &'static str {
        "立即在下一轮 turn 之前对历史对话执行 Summarize 压缩，释放上下文空间。\
         参数 reason 用于审计。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "为什么主动 compact（如：上下文接近 soft 阈值，准备开始新分析）"
                }
            }
        })
    }

    async fn execute(
        &self,
        input: Value,
        _ctx: &ToolContext,
    ) -> (Vec<ToolResultContent>, bool) {
        let reason = input
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        tracing::info!(
            target = "agent.tools.compact_now",
            reason = %reason,
            "agent 主动 compact"
        );
        // 实际 compact 由 loop 在下一轮 turn 入口检测此 ToolUse 后触发。
        (
            ok_json(json!({
                "ack": true,
                "reason": reason,
                "message": "下一轮 turn 将执行 Summarize 压缩"
            })),
            false,
        )
    }
}
