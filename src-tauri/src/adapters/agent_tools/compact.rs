//! `compact_now`——agent 主动管理 context 的工具。
//!
//! 设计：tool execute 本身**不做**实际压缩（防止 Tool trait 持有 provider 句柄），
//! 只在 ToolResult 文本里返回"已请求"+ 标记。pipeline::agent::loop_ 在下轮 turn
//! 启动时识别 compact_now 工具调用，把 `force_summarize_next_turn` 置 true，
//! 强制跑 Summarize tier 一次——不需要等撞 trigger_threshold。
//!
//! 适用场景（agent 主动判断）：
//! - 调研阶段累积大量 K 线 + scan_market + search_news 数据后，准备转入决策
//! - 一段长对话已经把要点交代完，想给后续讨论腾空间
//! - 看到 system prompt 提示"context 接近 soft 上限"主动收尾
//!
//! 实际效果：下一轮 run_one_turn 前会调 summarize_messages，把老对话压成 6 段中文
//! 摘要 + 边界 user 消息——agent 看到的 messages 列表变短，token 释放。

use crate::domain::agent::types::ToolResultContent;
use crate::pipeline::agent::tools::{ok_json, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::AppHandle;

/// 此工具被调用时返回的 marker 字符串——loop 通过工具 name == "compact_now" 检测，
/// 不依赖文本内容。这个文本只是给 agent 看的人话反馈。
const COMPACT_REQUESTED_MARKER: &str = "compact_now 已请求——下一轮 turn 将强制触发 Summarize tier，老对话会被压成 6 段索引";

pub struct CompactNowTool {
    #[allow(dead_code)] // 未来扩展可能用到 app（如直接读 token budget 配置）
    app: AppHandle,
}

impl CompactNowTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for CompactNowTool {
    fn name(&self) -> &'static str {
        "compact_now"
    }

    fn description(&self) -> &'static str {
        "主动压缩老对话释放 context 空间。调用大量工具（K 线 / scan_market / 资讯）\
        累积大数据后、要进决策阶段前调一下。下一轮 turn 自动跑 Summarize tier。\
        reason 简短说明为什么需要压缩（≤80 字，UI 复盘看）。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "≤80 字，例：'调研阶段累积 5 个 K 线 + 3 个资讯，转入决策前清理'"
                }
            },
            "required": ["reason"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let reason = input
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("(无说明)");
        let trimmed: String = reason.chars().take(80).collect();
        let payload = json!({
            "status": "compact_requested",
            "reason": trimmed,
            "note": COMPACT_REQUESTED_MARKER,
        });
        (ok_json(payload), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_contains_expected_keyword() {
        // loop_ 通过工具 name 检测 compact_now，不依赖 marker 文本——但
        // 保证文本里包含 "Summarize" 让 agent 看到时清楚发生了什么
        assert!(COMPACT_REQUESTED_MARKER.contains("Summarize"));
    }
}
