//! ToolSpec / ToolCall / ToolRegistry 协议层类型。
//!
//! Spec: docs/design/agent-infra-module.md §2 `ToolSpec` `ToolCall`，§5 Tool Registry API
//!
//! Infra 只定义注册协议；具体工具集合由 Runtime / adapter 注册。

use crate::domain::shared::{ErrorCode, OccurredAt};
use serde::{Deserialize, Serialize};
use specta::Type;

use super::messages::JsonSummary;

/// 工具副作用分类（spec §2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ToolSideEffect {
    None,
    NonTradingWrite,
    TradingWrite,
}

/// 工具来源 — local registry 或 provider server-side（spec §2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallSource {
    LocalTool,
    ServerSideTool,
}

/// 一个 local tool 的协议描述。
///
/// Spec: agent-infra-module.md §2 `ToolSpec`
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema（spec §2 inputSchema 必填）。
    pub input_schema: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    /// `source = "local_tool"`，常量；保留字段防止 wire 漂移。
    pub source: ToolCallSource,
    pub timeout_ms: u64,
    pub side_effect: ToolSideEffect,
}

impl ToolSpec {
    pub fn new_local(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: serde_json::Value,
        timeout_ms: u64,
        side_effect: ToolSideEffect,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            output_schema: None,
            source: ToolCallSource::LocalTool,
            timeout_ms,
            side_effect,
        }
    }
}

/// ToolCall id — provider 用，本仓库以字符串持有。
pub type ToolCallId = String;

/// 一次工具调用审计。
///
/// Spec: agent-infra-module.md §2 `ToolCall`
///
/// 规则：
/// - `name`：`source = "local_tool"` 必须是本次 registry 已注册名；server side 用 provider 原始工具名。
/// - 只读工具可只保存 summary；需要恢复副作用的工具（如 `operate_account`）必须保存结构化 payload，
///   通过 `inputPayloadRef` / `outputPayloadRef` 关联。
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool_call_id: ToolCallId,
    pub run_id: String,
    pub name: String,
    pub source: ToolCallSource,
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
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ErrorCode>,
    pub duration_ms: u64,
    /// 完整 payload 持久化引用；只读工具可省略，副作用工具必须有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_payload_ref: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn tool_spec_serializes_camel_case() {
        let s = ToolSpec::new_local(
            "fetch_quote",
            "Read latest quote",
            serde_json::json!({"type":"object"}),
            5000,
            ToolSideEffect::None,
        );
        let j = serde_json::to_value(&s).unwrap();
        assert_eq!(j["name"], "fetch_quote");
        assert_eq!(j["inputSchema"]["type"], "object");
        assert_eq!(j["source"], "local_tool");
        assert_eq!(j["sideEffect"], "none");
        assert_eq!(j["timeoutMs"], 5000);
    }

    #[test]
    fn tool_call_serde_roundtrip() {
        let c = ToolCall {
            tool_call_id: "tc1".into(),
            run_id: "r1".into(),
            name: "fetch_quote".into(),
            source: ToolCallSource::LocalTool,
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
            tool_call_id: "tc2".into(),
            run_id: "r1".into(),
            name: "do_x".into(),
            source: ToolCallSource::LocalTool,
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
