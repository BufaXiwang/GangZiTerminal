//! Pipeline 层构造的 [`AgentRequest`]——传给 `pipeline::agent::loop_::run_agent`。
//!
//! 所有 prompt 拼装、cache 边界打点、工具列表组装都集中在 pipeline 完成，
//! provider 拿到的是已经准备好的 canonical 形态。

use super::wire::{is_false, Message, SystemBlock};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 一个模型渠道使用的 provider wire format。
///
/// 跨 pipeline 配置、provider probe、具体 provider 构造共享的领域枚举。
/// serde rename 显式给出，避免 `OpenAI` 被自动拆成 `open_a_i_*`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderKind {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai_responses")]
    OpenAIResponses,
    #[serde(rename = "openai_chat_completions")]
    OpenAIChatCompletions,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::OpenAIResponses => "openai_responses",
            ProviderKind::OpenAIChatCompletions => "openai_chat_completions",
        }
    }
}

impl Default for ProviderKind {
    fn default() -> Self {
        ProviderKind::Anthropic
    }
}

/// 工具定义。本地工具走 `Local`；provider 替我们执行的走 `ServerSide`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolDef {
    /// 本地实现的工具——loop 收到 ToolUse 后调 ToolRegistry.dispatch。
    Local {
        name: String,
        description: String,
        /// JSON Schema 描述 input。
        input_schema: Value,
        /// 在工具定义末尾打 cache breakpoint（一般打在最后一个本地工具上，
        /// 把 identity + 全部工具定义一起缓存）。
        #[serde(skip_serializing_if = "is_false", default)]
        cache_control: bool,
    },
    /// Provider 内置工具——只声明，不本地执行。AnthropicProvider 翻译成
    /// `{"type": "web_search_20250305", ...}` 等专用形态。
    ServerSide(ServerSideTool),
}

/// 各 provider 内置的服务器端工具。新增工具往这里加 variant，
/// AnthropicProvider 的 serialize 路径同步处理即可。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerSideTool {
    /// Anthropic 原生 web_search_20250305。max_uses 限制单次请求内的搜索次数。
    /// allowed_domains / blocked_domains 留空表示不限制。
    AnthropicWebSearch {
        /// 暴露给 agent 的工具名，建议固定为 "web_search"。
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_uses: Option<u32>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        allowed_domains: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        blocked_domains: Vec<String>,
    },
}

/// Pipeline 层构造的请求，传给 `pipeline::agent::loop_::run_agent`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRequest {
    /// system blocks，按位置拼接。建议布局：identity → 静态指令 → 动态上下文，
    /// 把 cache_control 打在 identity+静态指令的末尾，让会话间共享 cache prefix。
    pub system: Vec<SystemBlock>,
    pub tools: Vec<ToolDef>,
    pub messages: Vec<Message>,
    pub options: AgentOptions,
    pub budget: ContextBudget,
    /// 可选的关联 id——落 agent_runs 表时用，便于后续按 chat_messages.id
    /// 反查这条 run 的成本和工具调用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_message_id: Option<String>,
    /// 这次 run 属于哪条 pipeline，落 agent_runs.pipeline 列。
    pub pipeline: PipelineKind,
}

/// 模型/采样/思考相关参数。每条 pipeline 独立配置，不共享。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOptions {
    /// provider-specific 模型 id，例如 "claude-sonnet-4-6" / "claude-opus-4-7"。
    pub model: String,
    pub max_tokens: u32,
    /// None 表示用 provider 默认温度。chat 偏 0.7、briefing/review 偏 0.3。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Thinking 配置。仅 Anthropic wire format 消费——OpenAI provider 忽略。
    /// `None` = 不开 thinking。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    /// effort 等级——控制 token 用量（含 thinking + tool call + text）。
    /// 4.6+ Anthropic 模型映射到 `output_config.effort`；OpenAI 模型当前未消费
    /// （OpenAI 用 channel 自己的 `reasoning_effort` 字段）。
    /// `None` = 不传字段（API 默认 high）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortLevel>,
    /// loop 内部的硬上限——超过这么多轮 tool-use 还没结束就强行截断。
    pub max_turns: u32,
    /// stop_sequences；一般留空。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    /// 单次 tool 调用的超时（秒）。超时返回 is_error=true，agent 下一轮可以补救。
    /// None 走默认 30s。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_secs: Option<u32>,
}

/// Anthropic thinking 配置。三种语义：
/// - **Adaptive**（推荐）：模型自己决定何时 + 多深 thinking。Opus 4.7 唯一支持模式，
///   4.6 系列推荐。配 [`AgentOptions::effort`] 控制深度。
/// - **Enabled**：手动给固定 budget_tokens。老模型（Sonnet 4.5、Opus 4.5、早期 4.x）
///   只支持这种；Opus 4.7 会被 wire format 层自动转 Adaptive。
///
/// Disabled 状态不在 enum 内——直接用 `Option<ThinkingConfig>` 的 `None` 表达。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ThinkingConfig {
    Adaptive {
        /// thinking 文本是否回流到 UI。Opus 4.7 API 默认 `omitted`（只返回 signature
        /// 节省延迟）；我们前端要渲染思考过程，默认强制 `summarized`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
    Enabled {
        budget_tokens: u32,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingDisplay {
    /// 返回模型 thinking 的摘要文本（含 signature 给多轮校验）。
    Summarized,
    /// 仅返回 signature，thinking 字段为空。首 token 更快但 UI 看不到思考内容。
    Omitted,
}

impl ThinkingDisplay {
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingDisplay::Summarized => "summarized",
            ThinkingDisplay::Omitted => "omitted",
        }
    }
}

/// Anthropic `output_config.effort`——控制 token 用量"努力等级"。
/// 适用于 Opus 4.7 / Opus 4.6 / Sonnet 4.6 / Mythos Preview。其他模型忽略字段。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    Max,
}

impl EffortLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            EffortLevel::Low => "low",
            EffortLevel::Medium => "medium",
            EffortLevel::High => "high",
            EffortLevel::XHigh => "xhigh",
            EffortLevel::Max => "max",
        }
    }
}

/// 上下文预算——超过 soft 触发摘要压缩，超过 hard 拒绝继续。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextBudget {
    pub soft_limit_tokens: u32,
    pub hard_limit_tokens: u32,
    /// 压缩时保留最近 N 轮 user/assistant 原文。
    pub compact_keep_last_n: u32,
    /// 单次 run 内允许的搜索调用上限（含本地 search_* 与 server-side web_search）。
    /// 防止 agent 把一次 briefing 搜成 20 次。
    pub max_search_calls: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum PipelineKind {
    Chat,
    Briefing,
    Review,
}

impl PipelineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PipelineKind::Chat => "chat",
            PipelineKind::Briefing => "briefing",
            PipelineKind::Review => "review",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_kind_string_round_trip() {
        assert_eq!(PipelineKind::Chat.as_str(), "chat");
        assert_eq!(PipelineKind::Briefing.as_str(), "briefing");
        assert_eq!(PipelineKind::Review.as_str(), "review");
    }
}
