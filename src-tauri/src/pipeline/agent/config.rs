//! Agent 运行时配置——**N 渠道模型**：用户可以加任意多个渠道，每个独立持有
//! `(wire_format + base_url + token + 可用模型清单)`；2 个 slot（chat / compact）
//! 各自分配一对 `(channel_id, model_id)`。v2 重构后 briefing / review 已下线。
//!
//! 存在 app_state 表的单 key `agent.config` 下，整个 JSON object 一次读写。
//! 前端 Settings 页通过 `get_agent_config` / `set_agent_config` 命令访问。
//!
//! Token 不做加密——SQLite 文件在 macOS 应用沙盒里，本地威胁模型基本是
//! "防止误传 git"，不是"防止本机攻击者读取磁盘"。

use crate::domain::agent::types::{
    EffortLevel, PipelineKind, ProviderKind, ThinkingConfig, ThinkingDisplay,
};
use crate::infrastructure::agent::provider::anthropic::{AnthropicConfig, AnthropicProvider};
use crate::infrastructure::agent::provider::openai::{
    OpenAIChatCompletionsConfig, OpenAIChatCompletionsProvider, OpenAIResponsesConfig,
    OpenAIResponsesProvider, ReasoningEffort,
};
use crate::infrastructure::agent::provider::retry::{RetryPolicy, RetryingProvider};
use crate::infrastructure::agent::provider::{ChatProvider, ProviderError};
use crate::infrastructure::app_state::{load_app_state_value, save_app_state_value};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tauri::AppHandle;

const AGENT_CONFIG_KEY: &str = "agent.config";

// ===== Channel ============================================================

/// 一个渠道——独立的 (wire_format, base_url, token, 可用模型清单, 协议层开关)。
/// 用户可以建任意多个，比如 "DeepSeek 个人"、"OpenAI 官方"、"火山方舟"……
/// 用 id 在 [`ModelRef`] 里被引用。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    /// 稳定 id（uuid 字符串），ModelRef 引用用。一旦生成不要改。
    pub id: String,
    /// 用户起的别名："DeepSeek 个人"、"老王 anthropic relay" 等——用于 UI 展示
    /// 和模型分配 dropdown 消歧（同一 model id 在多个渠道里时靠 name 区分）。
    pub name: String,
    /// 这个渠道用哪种 wire format。
    pub wire_format: ProviderKind,
    pub base_url: String,
    pub token: String,
    /// 该渠道下用户已 verify 通过的可用模型 id 列表。模型分配只能从这里选。
    #[serde(default)]
    pub available_models: Vec<String>,

    // ----- wire-format 专属字段（其他 wire format 忽略）-----
    /// Anthropic：原生 web_search 工具
    #[serde(default)]
    pub enable_native_web_search: bool,
    /// Anthropic：thinking 模式。默认 [`ThinkingMode::Adaptive`]——文档推荐 4.6+
    /// 全部走 adaptive。Haiku 实际会被 wire format 层 drop，无副作用。
    #[serde(default)]
    pub thinking_mode: ThinkingMode,
    /// 仅 [`ThinkingMode::Enabled`] 时使用——manual budget。Adaptive 模式忽略。
    #[serde(default = "default_thinking_budget")]
    pub thinking_budget_tokens: u32,
    /// Adaptive 模式下 thinking 文本是否回流到 UI。默认 Summarized 让 Opus 4.7 也能
    /// 流出思考过程（API 默认 omitted，UI 看不到）。
    #[serde(default = "default_thinking_display")]
    pub thinking_display: ThinkingDisplay,
    /// 默认 effort 等级。pipeline 没有 override 时用这个。`None` = 不传字段（API 默认 high）。
    /// Anthropic 走 `output_config.effort`；OpenAI Responses/Chat 用单独的
    /// [`Self::reasoning_effort`] 字段（语义不同）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<EffortLevel>,
    /// OpenAI：reasoning effort（gpt-5/o3 系列识别）。和 Anthropic 的 effort 是
    /// 两套 API，故分别表达。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// OpenAI Responses：内置 web_search 工具
    #[serde(default)]
    pub enable_web_search: bool,
}

/// Anthropic thinking 模式。canonical [`ThinkingConfig`] 的"配置形态"对偶——
/// 加 `Disabled` 是因为 channel 配置层需要"关闭"的显式表达，wire format 层用
/// `Option<ThinkingConfig>::None` 即可。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingMode {
    /// 推荐——模型自决何时 + 多深思考。配 [`Channel::default_effort`] 控制深度。
    /// Opus 4.7 唯一可用模式；Opus 4.6 / Sonnet 4.6 也推荐。
    #[default]
    Adaptive,
    /// Manual budget。老模型（Sonnet 4.5、Opus 4.5、早期 4.x）唯一可用模式。
    /// Opus 4.7 上会被 wire format 层自动转 Adaptive + warn。
    Enabled,
    /// 关闭 thinking。chat 历史压缩 / haiku 等快速场景。
    Disabled,
}

fn default_thinking_display() -> ThinkingDisplay {
    ThinkingDisplay::Summarized
}

impl Channel {
    #[allow(dead_code)] // 测试构造器；production 通过 set_agent_config 反序列化建
    pub fn new(name: impl Into<String>, wire_format: ProviderKind) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            wire_format,
            base_url: String::new(),
            token: String::new(),
            available_models: Vec::new(),
            enable_native_web_search: false,
            thinking_mode: ThinkingMode::default(),
            thinking_budget_tokens: default_thinking_budget(),
            thinking_display: default_thinking_display(),
            default_effort: None,
            reasoning_effort: None,
            enable_web_search: false,
        }
    }

    /// 把 channel 上的 thinking 配置翻译成 canonical [`ThinkingConfig`]。
    /// 返回 `None` 表示这个 channel 不开 thinking。
    pub fn thinking_config(&self) -> Option<ThinkingConfig> {
        match self.thinking_mode {
            ThinkingMode::Adaptive => Some(ThinkingConfig::Adaptive {
                display: Some(self.thinking_display),
            }),
            ThinkingMode::Enabled => Some(ThinkingConfig::Enabled {
                budget_tokens: self.thinking_budget_tokens,
            }),
            ThinkingMode::Disabled => None,
        }
    }
}

// ===== ModelRef + Assignments =============================================

/// 引用某个渠道下的某个模型。pipeline 用这个跑——查渠道拿 wire_format/url/token
/// 构 provider，然后用 model 字段当 model id。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    pub channel_id: String,
    pub model: String,
}

impl ModelRef {
    pub fn is_empty(&self) -> bool {
        self.channel_id.is_empty() || self.model.is_empty()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineAssignments {
    #[serde(default)]
    pub chat: ModelRef,
    /// 用于 chat 上下文压缩（Summarize tier）的便宜模型。
    #[serde(default)]
    pub compact: ModelRef,
}

// ===== AgentConfig 顶层 ===================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfig {
    /// 用户自定义渠道列表。可以有多个相同 wire_format 的渠道（不同 base_url/token）。
    #[serde(default)]
    pub channels: Vec<Channel>,
    /// 4 个 pipeline 各自的 (channel, model) 分配。
    #[serde(default)]
    pub assignments: PipelineAssignments,
    pub agent: AgentRuntimeConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            channels: Vec::new(),
            assignments: PipelineAssignments::default(),
            agent: AgentRuntimeConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRuntimeConfig {
    pub max_turns_per_run: u32,
    pub max_search_calls_per_run: u32,
    pub context_soft_limit_tokens: u32,
    pub context_hard_limit_tokens: u32,
    pub compact_keep_last_n_turns: u32,
    #[serde(default = "default_tool_timeout_secs")]
    pub tool_timeout_secs: u32,
    #[serde(default = "default_summarize_threshold")]
    pub context_summarize_threshold: u32,
    #[serde(default = "default_summarize_max_failures")]
    pub summarize_max_consecutive_failures: u32,
}

fn default_tool_timeout_secs() -> u32 {
    30
}
fn default_summarize_threshold() -> u32 {
    150_000
}
fn default_summarize_max_failures() -> u32 {
    3
}
fn default_thinking_budget() -> u32 {
    4_000
}

impl Default for AgentRuntimeConfig {
    fn default() -> Self {
        Self {
            max_turns_per_run: 12,
            max_search_calls_per_run: 5,
            // 主流 Claude 4.x / GPT-5 / 国内强模型基本都 ≥ 200k context；之前 80k/160k
            // 是 Sonnet 3.5 时代留下的保守值。提到 130k/190k 给响应留 ~10k buffer。
            context_soft_limit_tokens: 130_000,
            context_hard_limit_tokens: 190_000,
            compact_keep_last_n_turns: 6,
            tool_timeout_secs: default_tool_timeout_secs(),
            context_summarize_threshold: default_summarize_threshold(),
            summarize_max_consecutive_failures: default_summarize_max_failures(),
        }
    }
}

impl AgentConfig {
    /// 按 id 找渠道——pipeline run / verify / build_provider 都靠这个。
    pub fn find_channel(&self, id: &str) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }

    /// 拿某个 pipeline 的 (channel, model) 引用。
    pub fn assignment_for(&self, pipeline: PipelineKind) -> &ModelRef {
        match pipeline {
            PipelineKind::Chat => &self.assignments.chat,
        }
    }

    /// 拿 compact (LLM 摘要) 用的 ModelRef。和 PipelineKind 平行——compact 不是
    /// 正经 pipeline，是 chat 内部 long-context 时的兜底。
    pub fn compact_assignment(&self) -> &ModelRef {
        &self.assignments.compact
    }

    /// 解析 (pipeline) → (Channel, model_id)。失败时返回明确的错误文案。
    pub fn resolve_pipeline(&self, pipeline: PipelineKind) -> Result<(&Channel, &str), String> {
        let r = self.assignment_for(pipeline);
        if r.is_empty() {
            return Err(format!(
                "{} pipeline 未分配模型——请到 设置 → 模型分配 选一个",
                pipeline.as_str()
            ));
        }
        let chan = self.find_channel(&r.channel_id).ok_or_else(|| {
            format!(
                "{} pipeline 引用了不存在的渠道 id={}（渠道被删了？请重新分配）",
                pipeline.as_str(),
                r.channel_id
            )
        })?;
        Ok((chan, r.model.as_str()))
    }

    pub fn resolve_compact(&self) -> Option<(&Channel, &str)> {
        let r = self.compact_assignment();
        if r.is_empty() {
            return None;
        }
        let chan = self.find_channel(&r.channel_id)?;
        Some((chan, r.model.as_str()))
    }

    /// chat pipeline 已经分配且渠道存在。
    pub fn ensure_ready(&self) -> Result<(), String> {
        self.resolve_pipeline(PipelineKind::Chat)?;
        // compact 不强求——chat 没撞 summarize threshold 不会用，缺了不阻塞 chat 启动
        Ok(())
    }
}

// ===== KV 存取 + 老配置迁移 ===============================================

pub fn read_agent_config(app: &AppHandle) -> AgentConfig {
    match load_app_state_value(app, AGENT_CONFIG_KEY) {
        Ok(Some(v)) => parse_with_migration(v),
        _ => AgentConfig::default(),
    }
}

/// v2 重构后 app_state 跟随 DB 一起备份重建——这里不需要兼容老 schema 的 config JSON。
/// 直接 deserialize，失败回 default。
fn parse_with_migration(v: Value) -> AgentConfig {
    serde_json::from_value::<AgentConfig>(v).unwrap_or_default()
}

pub fn write_agent_config(app: &AppHandle, cfg: &AgentConfig) -> Result<(), String> {
    let v = serde_json::to_value(cfg).map_err(|e| format!("agent config 序列化失败：{e}"))?;
    save_app_state_value(app, AGENT_CONFIG_KEY, &v)
}

// Tauri 命令（get/set_agent_config + verify_provider_model）+ token mask helpers
// 已移到 `adapters::agent_commands`——IPC 边界专属。本文件留 business logic。

// ===== Provider 工厂 ======================================================

/// 按某个渠道构造对应 ChatProvider 实例。出口套一层 RetryingProvider。
pub fn build_provider_for_channel(chan: &Channel) -> Result<Arc<dyn ChatProvider>, ProviderError> {
    let inner: Arc<dyn ChatProvider> = match chan.wire_format {
        ProviderKind::Anthropic => {
            let p = AnthropicProvider::new(AnthropicConfig::new(
                chan.base_url.clone(),
                chan.token.clone(),
            ))?;
            Arc::new(p)
        }
        ProviderKind::OpenAIResponses => {
            let mut c = OpenAIResponsesConfig::new(chan.base_url.clone(), chan.token.clone());
            c.reasoning_effort = chan.reasoning_effort;
            c.enable_web_search = chan.enable_web_search;
            let p = OpenAIResponsesProvider::new(c)?;
            Arc::new(p)
        }
        ProviderKind::OpenAIChatCompletions => {
            let mut c = OpenAIChatCompletionsConfig::new(chan.base_url.clone(), chan.token.clone());
            c.reasoning_effort = chan.reasoning_effort;
            let p = OpenAIChatCompletionsProvider::new(c)?;
            Arc::new(p)
        }
    };
    Ok(Arc::new(RetryingProvider::new(
        inner,
        RetryPolicy::default(),
    )))
}

// ===== 测试 ===============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_config_is_empty() {
        let cfg = AgentConfig::default();
        assert!(cfg.channels.is_empty());
        assert!(cfg.assignments.chat.is_empty());
    }

    #[test]
    fn ensure_ready_fails_without_assignment() {
        let cfg = AgentConfig::default();
        let err = cfg.ensure_ready().unwrap_err();
        assert!(err.contains("未分配"), "got {err}");
    }

    #[test]
    fn assign_then_resolve() {
        let mut cfg = AgentConfig::default();
        let mut chan = Channel::new("DeepSeek", ProviderKind::OpenAIChatCompletions);
        chan.base_url = "https://api.deepseek.com".into();
        chan.token = "sk-x".into();
        chan.available_models = vec!["deepseek-chat".into(), "deepseek-reasoner".into()];
        let chan_id = chan.id.clone();
        cfg.channels.push(chan);
        cfg.assignments.chat = ModelRef {
            channel_id: chan_id.clone(),
            model: "deepseek-chat".into(),
        };
        assert!(cfg.ensure_ready().is_ok());
        let (chan_ref, model) = cfg.resolve_pipeline(PipelineKind::Chat).unwrap();
        assert_eq!(chan_ref.id, chan_id);
        assert_eq!(model, "deepseek-chat");
    }

    #[test]
    fn resolve_fails_when_channel_missing() {
        let mut cfg = AgentConfig::default();
        cfg.assignments.chat = ModelRef {
            channel_id: "ghost".into(),
            model: "x".into(),
        };
        let err = cfg.resolve_pipeline(PipelineKind::Chat).unwrap_err();
        assert!(err.contains("不存在的渠道"), "got {err}");
    }
}
