//! Agent 子系统的核心类型契约。
//!
//! 设计原则：
//! - 内部消息以 Anthropic content-block 形态作为 canonical（最具表达力的超集：
//!   text / thinking / image / tool_use / tool_result）。其他 provider（未来 OpenAI、
//!   本地模型）需要把自己的协议翻译到这套形态。
//! - 所有结构体 serde 双向，便于直接落库 / 通过 Tauri emit 给前端。
//! - Pipeline 层只构造 [`AgentRequest`]，所有 prompt 拼装、cache 边界打点、
//!   工具列表组装都集中在这里完成。

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ===== 基础消息形态 ======================================================

/// 消息角色。System 单独走 [`AgentRequest::system`]，messages 列表里只放 user/assistant。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// 一条消息由若干 content block 组成。这是 canonical 的中间表示——
/// AnthropicProvider 序列化时直接对应 `content: [...]`；未来 OpenAIProvider
/// 需要把这套结构降级翻译。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Block>,
}

/// content block 的全部类型。`#[serde(tag = "type")]` 让 JSON 形态贴近 Anthropic 协议。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    /// 普通文本片段。
    Text {
        text: String,
        /// cache_control 默认 false；只在打 cache breakpoint 的最后一个 block 上置 true。
        #[serde(skip_serializing_if = "is_false", default)]
        cache_control: bool,
    },

    /// Extended thinking 块（Anthropic）。signature 由 provider 写入，
    /// 跨轮次回传时必须原样带回，否则签名校验会失败。
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },

    /// Redacted thinking——被服务器加密的思考块。loop 必须原样转发，不能丢。
    RedactedThinking { data: String },

    /// 图片。data 是 base64，mime 形如 image/png / image/jpeg。
    Image { mime: String, data: String },

    /// Agent 调本地工具。loop 看到此 block 应执行 ToolRegistry.dispatch，
    /// 把结果作为 ToolResult 拼回下一轮 messages。
    /// `server_side=true` 表示这是 provider 替我们执行的（Anthropic web_search_20250305 等）——
    /// loop 不要执行，等 provider 在同一回合内回填 tool_result 即可。
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(skip_serializing_if = "is_false", default)]
        server_side: bool,
    },

    /// 工具执行结果。`server_side=true` 时 content 由 provider 填充并原样转发。
    ToolResult {
        tool_use_id: String,
        content: Vec<ToolResultContent>,
        #[serde(skip_serializing_if = "is_false", default)]
        is_error: bool,
        #[serde(skip_serializing_if = "is_false", default)]
        server_side: bool,
        /// 同 Block::Text 的语义。
        #[serde(skip_serializing_if = "is_false", default)]
        cache_control: bool,
    },
}

/// tool_result 的 content 通常是文本，偶尔是图（截图工具）。
/// Anthropic 也允许嵌套结构化对象（server-side tool 的原始返回），用 Json 兜底。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContent {
    Text {
        text: String,
    },
    Image {
        mime: String,
        data: String,
    },
    /// 对 server-side 工具（如 Anthropic web_search_tool_result）原样透传。
    /// 字段名带 _raw 提示这是绕过 canonical 抽象的逃生舱。
    Json {
        raw: Value,
    },
}

// ===== System / 工具定义 =================================================

/// system 字段的 block。多段拼接，按位置打 cache_control 形成 cache prefix 链。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemBlock {
    pub text: String,
    /// 在该 block 末尾打 cache breakpoint。整次请求最多 4 个 breakpoint
    /// （含 tools 区与 messages 区里的），超过会被 provider 拒绝。
    #[serde(skip_serializing_if = "is_false", default)]
    pub cache_control: bool,
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

// ===== 请求入参 ==========================================================

/// Pipeline 层构造的请求，传给 [`crate::pipeline::agent::loop_::run_agent`]。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRequest {
    /// system blocks，按位置拼接。建议布局：identity → 静态指令 → 动态上下文，
    /// 把 cache_control 打在 identity+静态指令的末尾，让会话间共享 cache prefix。
    pub system: Vec<SystemBlock>,
    pub tools: Vec<ToolDef>,
    pub messages: Vec<Message>,
    pub options: AgentOptions,
    pub budget: ContextBudget,
    /// 可选的关联 id——落 agent_runs 表时用，便于后续按 chat_messages.id /
    /// analysis_records.id 反查这条 run 的成本和工具调用。
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

// ===== 事件流 ============================================================

/// run_agent 流式输出的事件。AnthropicProvider 把 SSE 翻译成这套；
/// loop 在工具执行前后追加 ToolStart/ToolEnd；observer 把流转发到 Tauri emit。
///
/// 前端 useChatMessageStream 直接消费这套，不感知 provider 协议差异。
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
    /// 直接丢弃最老消息——兜底。
    Drop,
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

// ===== 序列化辅助 ========================================================

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

// ===== 单元测试 ==========================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn block_text_round_trips() {
        let block = Block::Text {
            text: "hi".into(),
            cache_control: true,
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(v["type"], "text");
        assert_eq!(v["text"], "hi");
        assert_eq!(v["cache_control"], true);
        let back: Block = serde_json::from_value(v).unwrap();
        match back {
            Block::Text {
                text,
                cache_control,
            } => {
                assert_eq!(text, "hi");
                assert!(cache_control);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn block_text_omits_default_cache_control() {
        let block = Block::Text {
            text: "hi".into(),
            cache_control: false,
        };
        let v = serde_json::to_value(&block).unwrap();
        assert!(
            v.get("cache_control").is_none(),
            "default cache_control 应该不序列化，避免污染请求体"
        );
    }

    #[test]
    fn tool_use_with_server_side_flag() {
        let block = Block::ToolUse {
            id: "toolu_1".into(),
            name: "web_search".into(),
            input: json!({"query": "茅台"}),
            server_side: true,
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(v["type"], "tool_use");
        assert_eq!(v["server_side"], true);
    }

    #[test]
    fn message_serializes_with_role_lowercase() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![Block::Text {
                text: "ok".into(),
                cache_control: false,
            }],
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["role"], "assistant");
    }

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

    #[test]
    fn pipeline_kind_string_round_trip() {
        assert_eq!(PipelineKind::Chat.as_str(), "chat");
        assert_eq!(PipelineKind::Briefing.as_str(), "briefing");
        assert_eq!(PipelineKind::Review.as_str(), "review");
    }
}
