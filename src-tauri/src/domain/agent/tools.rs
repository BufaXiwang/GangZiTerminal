//! ToolSpec / ToolCall — Tool 协议层类型。
//!
//! Spec: docs/design/agent-infra-module.md §2 `ToolSpec` `ToolCall`，§5 Tool Registry API
//!
//! 设计：
//! - Infra 只定义注册协议，不规定产品里必须有哪些 tool。
//! - Tool 调用通过 `<use_tool name="...">{...}</use_tool>` XML 文本协议；不走 provider 原生 tool_use。
//! - ToolCall 是审计真源；chat 历史中的 `<tool_result>` XML 是给 LLM / 用户看的副本。

use crate::domain::shared::{ErrorCode, OccurredAt};
use serde::{Deserialize, Serialize};
use specta::Type;

use super::messages::JsonSummary;

/// Tool 副作用分类（spec §2 `SideEffect`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SideEffect {
    None,
    NonTradingWrite,
    TradingWrite,
}

/// 一个 Tool 的协议描述。
///
/// Spec: agent-infra-module.md §2 `ToolSpec`
///
/// 规则（来自 spec §2）：
/// - 同名 tool 只能注册一次；重复注册必须 fail closed。
/// - `examples` 至少 1 个完整 `<use_tool ...>{...}</use_tool>` 示例字符串。
/// - description 应当能被产品负责人手写为 markdown。
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema（dispatch 前校验）。
    pub input_schema: serde_json::Value,
    pub examples: Vec<String>,
    pub side_effect: SideEffect,
    pub timeout_ms: u64,
    /// fork 类工具标记（`run_subagent` / `run_skill` 等）。
    ///
    /// 构造子 run registry 时**按此剔除**（spec §3.5 无嵌套，不靠 name 白名单），
    /// 确保任何新增 fork 类工具（含 Runtime 注入的领域 fork 工具）都被覆盖。缺省 false。
    #[serde(default)]
    pub is_spawn: bool,
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: serde_json::Value,
        examples: Vec<String>,
        timeout_ms: u64,
        side_effect: SideEffect,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            examples,
            side_effect,
            timeout_ms,
            is_spawn: false,
        }
    }

    /// 标记为 fork/spawn 类工具（子 agent registry 构造时被剔除）。
    pub fn spawn(mut self) -> Self {
        self.is_spawn = true;
        self
    }
}

/// ToolCall id — Infra 在 parser 检测到 `<use_tool>` 闭合时生成（`tc_<uuid>`）。
pub type ToolCallId = String;

/// 一次 tool 调用审计。
///
/// Spec: agent-infra-module.md §2 `ToolCall`
///
/// PayloadStore 双层存储规则：
/// - input / output JSON 序列化后 ≤ 8KB 时 `inputSummary` = 完整 payload，`inputPayloadRef = None`。
/// - > 8KB 时 `inputSummary` 是截断摘要（前 1KB + `"[truncated, see ref]"`），完整数据通过 ref。
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool_call_id: ToolCallId,
    pub run_id: String,
    pub name: String,
    pub input_summary: JsonSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_payload_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_summary: Option<JsonSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_payload_ref: Option<String>,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ErrorCode>,
    pub started_at: OccurredAt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<OccurredAt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// Tool dispatch 的最终结果（Infra 不解释业务语义）。
///
/// Spec: agent-infra-module.md §5 Tool Registry API
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallResult {
    pub tool_call_id: ToolCallId,
    pub output_summary: JsonSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_payload_ref: Option<String>,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ErrorCode>,
    pub duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn tool_spec_serializes_camel_case() {
        let s = ToolSpec::new(
            "fetch_quote",
            "Read latest quote",
            serde_json::json!({"type":"object"}),
            vec![r#"<use_tool name="fetch_quote">{"tsCode":"600519.SH"}</use_tool>"#.into()],
            5000,
            SideEffect::None,
        );
        let j = serde_json::to_value(&s).unwrap();
        assert_eq!(j["name"], "fetch_quote");
        assert_eq!(j["inputSchema"]["type"], "object");
        assert_eq!(j["sideEffect"], "none");
        assert_eq!(j["timeoutMs"], 5000);
        assert!(j["examples"][0].as_str().unwrap().starts_with("<use_tool"));
    }

    #[test]
    fn tool_call_serde_roundtrip() {
        let c = ToolCall {
            tool_call_id: "tc_abc".into(),
            run_id: "r1".into(),
            name: "fetch_quote".into(),
            input_summary: serde_json::json!({"tsCode":"600519.SH"}),
            input_payload_ref: None,
            output_summary: Some(serde_json::json!({"price":"100.5"})),
            output_payload_ref: None,
            is_error: false,
            error_code: None,
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
            duration_ms: Some(120),
        };
        let j = serde_json::to_string(&c).unwrap();
        let back: ToolCall = serde_json::from_str(&j).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn tool_call_error_serializes_error_code() {
        let c = ToolCall {
            tool_call_id: "tc_x".into(),
            run_id: "r1".into(),
            name: "do_x".into(),
            input_summary: serde_json::json!({}),
            input_payload_ref: None,
            output_summary: Some(serde_json::json!({"reason":"insufficient_cash"})),
            output_payload_ref: None,
            is_error: true,
            error_code: Some(ErrorCode::InsufficientCash),
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
            duration_ms: Some(5),
        };
        let j = serde_json::to_value(&c).unwrap();
        assert_eq!(j["isError"], true);
        assert_eq!(j["errorCode"], "insufficient_cash");
    }
}
