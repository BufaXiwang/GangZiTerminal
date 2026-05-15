//! Agent 运行时配置——**N 渠道模型**：用户可以加任意多个渠道，每个独立持有
//! `(wire_format + base_url + token + 可用模型清单)`；4 个 pipeline slot
//! （chat / briefing / review / compact）各自分配一对 `(channel_id, model_id)`，
//! 互不影响。
//!
//! 老配置（3 槽固定 anthropic / openaiResponses / openaiChatCompletions）通过
//! `migrate_legacy_three_channel_config` 自动转成 channels 数组，第一次写库后
//! 老字段消失。
//!
//! 存在 app_state 表的单 key `agent.config` 下，整个 JSON object 一次读写。
//! 前端 Settings 页通过 `get_agent_config` / `set_agent_config` 命令访问。
//!
//! Token 不做加密——SQLite 文件在 macOS 应用沙盒里，本地威胁模型基本是
//! "防止误传 git"，不是"防止本机攻击者读取磁盘"。

use crate::agent::provider::anthropic::{AnthropicConfig, AnthropicProvider};
use crate::agent::provider::openai::{
    OpenAIChatCompletionsConfig, OpenAIChatCompletionsProvider, OpenAIResponsesConfig,
    OpenAIResponsesProvider, ReasoningEffort,
};
use crate::agent::provider::retry::{RetryPolicy, RetryingProvider};
use crate::agent::provider::{ChatProvider, ProviderError};
use crate::agent::types::{EffortLevel, PipelineKind, ThinkingConfig, ThinkingDisplay};
use crate::db::{load_app_state_value, save_app_state_value};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tauri::AppHandle;

const AGENT_CONFIG_KEY: &str = "agent.config";

// ===== Wire format 枚举 ===================================================

/// 一个渠道使用的 wire format。
///
/// serde rename 显式给——`rename_all = "snake_case"` 对 `OpenAI` 这种连续大写
/// 缩写会拆成 `open_a_i_*`，不是我们要的形态。
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
    #[serde(default)]
    pub briefing: ModelRef,
    #[serde(default)]
    pub review: ModelRef,
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
    120_000
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
            context_soft_limit_tokens: 80_000,
            context_hard_limit_tokens: 160_000,
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
            PipelineKind::Briefing => &self.assignments.briefing,
            PipelineKind::Review => &self.assignments.review,
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

    /// 全部 pipeline + compact 都已经分配且渠道存在。
    pub fn ensure_ready(&self) -> Result<(), String> {
        for pipe in [
            PipelineKind::Chat,
            PipelineKind::Briefing,
            PipelineKind::Review,
        ] {
            self.resolve_pipeline(pipe)?;
        }
        // compact 不强求——chat 没撞 summarize threshold 不会用，缺了不阻塞 chat 启动
        Ok(())
    }

    /// 当前 chat pipeline 是否启用 server-side web_search——按 chat 分配的渠道决定。
    pub fn web_search_enabled_for_chat(&self) -> bool {
        let Ok((chan, _)) = self.resolve_pipeline(PipelineKind::Chat) else {
            return false;
        };
        match chan.wire_format {
            ProviderKind::Anthropic => chan.enable_native_web_search,
            ProviderKind::OpenAIResponses => chan.enable_web_search,
            ProviderKind::OpenAIChatCompletions => false,
        }
    }
}

// ===== KV 存取 + 老配置迁移 ===============================================

pub fn read_agent_config(app: &AppHandle) -> AgentConfig {
    match load_app_state_value(app, AGENT_CONFIG_KEY) {
        Ok(Some(v)) => parse_with_migration(v),
        _ => AgentConfig::default(),
    }
}

fn parse_with_migration(mut v: Value) -> AgentConfig {
    // 已经是新形态（含 channels 字段）→ 直接 deserialize
    if v.get("channels").is_some() {
        return serde_json::from_value::<AgentConfig>(v).unwrap_or_default();
    }
    // 老形态——迁移到 channels[] + assignments
    if let Some(migrated) = migrate_legacy_three_channel_config(&mut v) {
        return migrated;
    }
    AgentConfig::default()
}

/// 老 schema 三种已见形态：
///   - 最早：`{ provider, anthropic: {...}, openai: {...}, agent }`
///   - 中间：`{ provider, anthropic: {...}, openaiResponses: {...},
///             openaiChatCompletions: {...}, agent }`
///   - 也可能字段都缺，此时 fall through
///
/// 都映射成 channels = [..1-3 条预设渠道..] + assignments。返回 None 表示这个
/// JSON 形态完全不认识，调用方该用 default。
fn migrate_legacy_three_channel_config(v: &mut Value) -> Option<AgentConfig> {
    let obj = v.as_object_mut()?;
    let mut channels: Vec<Channel> = Vec::new();
    let mut id_anthropic: Option<String> = None;
    let mut id_openai_resp: Option<String> = None;
    let mut id_openai_chat: Option<String> = None;

    if let Some(anth) = obj.get("anthropic") {
        let mut chan = Channel::new("Anthropic", ProviderKind::Anthropic);
        copy_str(&mut chan.base_url, anth, "baseUrl");
        copy_str(&mut chan.token, anth, "token");
        copy_string_array(&mut chan.available_models, anth, "availableModels");
        copy_bool(
            &mut chan.enable_native_web_search,
            anth,
            "enableNativeWebSearch",
        );
        copy_u32(
            &mut chan.thinking_budget_tokens,
            anth,
            "thinkingBudgetTokens",
        );
        // 老配置只有 enableThinking: bool——true 对应 Enabled，false 对应 Disabled。
        // 新装机 / 未设字段保持 Adaptive 默认。
        if let Some(legacy_enabled) = anth.get("enableThinking").and_then(Value::as_bool) {
            chan.thinking_mode = if legacy_enabled {
                ThinkingMode::Enabled
            } else {
                ThinkingMode::Disabled
            };
        }
        id_anthropic = Some(chan.id.clone());
        channels.push(chan);
    }
    // 老的 "openai" 字段（在 openai_responses / chat_completions 拆分前）
    let legacy_openai = obj.get("openai").cloned();
    let openai_resp_src = obj
        .get("openaiResponses")
        .cloned()
        .or_else(|| legacy_openai.clone());
    let openai_chat_src = obj.get("openaiChatCompletions").cloned().or(legacy_openai);

    if let Some(o) = openai_resp_src.as_ref() {
        let mut chan = Channel::new("OpenAI Responses", ProviderKind::OpenAIResponses);
        copy_str(&mut chan.base_url, o, "baseUrl");
        copy_str(&mut chan.token, o, "token");
        copy_string_array(&mut chan.available_models, o, "availableModels");
        copy_bool(&mut chan.enable_web_search, o, "enableWebSearch");
        if let Some(re) = o
            .get("reasoningEffort")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
        {
            chan.reasoning_effort = re;
        }
        id_openai_resp = Some(chan.id.clone());
        channels.push(chan);
    }
    if let Some(o) = openai_chat_src.as_ref() {
        let mut chan = Channel::new("OpenAI Chat", ProviderKind::OpenAIChatCompletions);
        copy_str(&mut chan.base_url, o, "baseUrl");
        copy_str(&mut chan.token, o, "token");
        copy_string_array(&mut chan.available_models, o, "availableModels");
        if let Some(re) = o
            .get("reasoningEffort")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
        {
            chan.reasoning_effort = re;
        }
        id_openai_chat = Some(chan.id.clone());
        channels.push(chan);
    }

    if channels.is_empty() {
        return None;
    }

    // assignments：用旧 provider 字段 + 对应渠道的 models map 填
    let provider_str = obj
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("anthropic");
    let active_id = match provider_str {
        "openai_responses" => id_openai_resp.clone(),
        "openai_chat_completions" => id_openai_chat.clone(),
        _ => id_anthropic.clone(),
    }
    .or(channels.first().map(|c| c.id.clone()))
    .unwrap_or_default();

    let provider_block = match provider_str {
        "openai_responses" => obj.get("openaiResponses").or(obj.get("openai")),
        "openai_chat_completions" => obj.get("openaiChatCompletions").or(obj.get("openai")),
        _ => obj.get("anthropic"),
    };
    let mk_ref = |slot: &str| -> ModelRef {
        let model = provider_block
            .and_then(|p| p.get("models"))
            .and_then(|m| m.get(slot))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if model.is_empty() {
            return ModelRef::default();
        }
        ModelRef {
            channel_id: active_id.clone(),
            model,
        }
    };
    let assignments = PipelineAssignments {
        chat: mk_ref("chat"),
        briefing: mk_ref("briefing"),
        review: mk_ref("review"),
        compact: mk_ref("compact"),
    };

    let agent: AgentRuntimeConfig = obj
        .get("agent")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    Some(AgentConfig {
        channels,
        assignments,
        agent,
    })
}

fn copy_str(dst: &mut String, src: &Value, key: &str) {
    if let Some(s) = src.get(key).and_then(Value::as_str) {
        *dst = s.to_string();
    }
}
fn copy_bool(dst: &mut bool, src: &Value, key: &str) {
    if let Some(b) = src.get(key).and_then(Value::as_bool) {
        *dst = b;
    }
}
fn copy_u32(dst: &mut u32, src: &Value, key: &str) {
    if let Some(n) = src.get(key).and_then(Value::as_u64) {
        *dst = n as u32;
    }
}
fn copy_string_array(dst: &mut Vec<String>, src: &Value, key: &str) {
    if let Some(arr) = src.get(key).and_then(Value::as_array) {
        *dst = arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
}

pub fn write_agent_config(app: &AppHandle, cfg: &AgentConfig) -> Result<(), String> {
    let v = serde_json::to_value(cfg).map_err(|e| format!("agent config 序列化失败：{e}"))?;
    save_app_state_value(app, AGENT_CONFIG_KEY, &v)
}

// ===== Tauri 命令 =========================================================

#[tauri::command]
pub fn get_agent_config(app: AppHandle) -> Value {
    let cfg = read_agent_config(&app);
    let mut value = serde_json::to_value(&cfg).unwrap_or(Value::Null);
    // 把每个渠道的 token 都 mask 掉
    if let Some(channels) = value.get_mut("channels").and_then(Value::as_array_mut) {
        for chan in channels.iter_mut() {
            if let Some(token) = chan
                .get("token")
                .and_then(Value::as_str)
                .map(str::to_string)
            {
                let masked = mask_token(&token);
                if let Some(t) = chan.as_object_mut().and_then(|m| m.get_mut("token")) {
                    *t = Value::String(masked);
                }
            }
        }
    }
    value
}

#[tauri::command]
pub fn set_agent_config(app: AppHandle, config: Value) -> Result<(), String> {
    let new_cfg: AgentConfig = serde_json::from_value(config.clone())
        .map_err(|e| format!("agent config 解析失败：{e}"))?;
    let mut cfg = new_cfg;
    let existing = read_agent_config(&app);
    // 每个渠道的 token：未编辑（空字符串 / mask 形态）时保留旧 token。
    // 按 channel id 匹配——id 未变就复用；新加渠道无对应旧 token。
    for chan in cfg.channels.iter_mut() {
        if let Some(old) = existing.find_channel(&chan.id) {
            let mask = mask_token(&old.token);
            if chan.token.trim().is_empty() || chan.token == mask {
                chan.token = old.token.clone();
            }
        }
    }
    write_agent_config(&app, &cfg)
}

fn mask_token(token: &str) -> String {
    if token.is_empty() {
        return String::new();
    }
    let len = token.chars().count();
    let prefix: String = token.chars().take(8.min(len)).collect();
    format!("{prefix}…({len} chars)")
}

fn resolve_token(incoming: &str, stored: &str) -> String {
    let mask = mask_token(stored);
    if incoming.trim().is_empty() || incoming == mask {
        stored.to_string()
    } else {
        incoming.to_string()
    }
}

fn resolve_base_url(incoming: &str, stored: &str) -> String {
    if incoming.trim().is_empty() {
        stored.to_string()
    } else {
        incoming.to_string()
    }
}

// ===== verify =============================================================

/// 校验某个 model id 在某个渠道下是否能跑通——发 1-token 探针。
///
/// 入参 `channelId` 是新结构的渠道 id；`baseUrl` / `token` 接受 mask 形态或空，
/// 自动回退到 stored 渠道里的值（前端"还没保存就想 verify"的情况）。
#[tauri::command]
pub async fn verify_provider_model(
    app: AppHandle,
    #[allow(non_snake_case)] channelId: String,
    #[allow(non_snake_case)] baseUrl: String,
    token: String,
    model: String,
) -> Result<(), String> {
    if model.trim().is_empty() {
        return Err("model 为空".into());
    }
    let stored = read_agent_config(&app);
    let stored_chan = stored.find_channel(&channelId);
    let (resolved_url, resolved_token, wire_format) = match stored_chan {
        Some(chan) => (
            resolve_base_url(&baseUrl, &chan.base_url),
            resolve_token(&token, &chan.token),
            chan.wire_format,
        ),
        None => {
            // 新建中、还没保存的渠道——前端必须自己把 wire_format 通过另一条命令传，
            // 当前为简化让 verify 失败提示。实际中 add 模型那一步会把渠道先存好，
            // 然后用 channelId 调这里。
            return Err(format!(
                "未找到 channel id={channelId}——请先保存渠道后再验证"
            ));
        }
    };
    if resolved_url.trim().is_empty() || resolved_token.trim().is_empty() {
        return Err("base_url 或 token 为空".into());
    }
    crate::agent::provider::models::verify_model(
        wire_format,
        &resolved_url,
        &resolved_token,
        &model,
    )
    .await
    .map_err(|e| format!("{e}"))
}

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
        cfg.assignments.briefing = ModelRef {
            channel_id: chan_id.clone(),
            model: "deepseek-reasoner".into(),
        };
        cfg.assignments.review = ModelRef {
            channel_id: chan_id.clone(),
            model: "deepseek-reasoner".into(),
        };
        // chat / briefing / review 都填了才 ensure_ready ok
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

    #[test]
    fn migrate_legacy_three_channel_works() {
        let v = json!({
            "provider": "anthropic",
            "anthropic": {
                "baseUrl": "https://anthro",
                "token": "cr_xxx",
                "availableModels": ["claude-opus-4-7", "claude-sonnet-4-6"],
                "models": {
                    "chat": "claude-sonnet-4-6",
                    "briefing": "claude-opus-4-7",
                    "review": "claude-opus-4-7",
                    "compact": "claude-haiku-4-5-20251001"
                },
                "enableThinking": true,
                "thinkingBudgetTokens": 6000
            },
            "openaiResponses": {
                "baseUrl": "https://openai",
                "token": "sk-test",
                "availableModels": ["gpt-5"],
                "models": {"chat": "", "briefing": "", "review": "", "compact": ""}
            },
            "agent": {
                "maxTurnsPerRun": 12,
                "maxSearchCallsPerRun": 5,
                "contextSoftLimitTokens": 80000,
                "contextHardLimitTokens": 160000,
                "compactKeepLastNTurns": 6,
                "toolTimeoutSecs": 30
            }
        });
        let cfg = parse_with_migration(v);
        assert_eq!(cfg.channels.len(), 2);
        // active provider 是 anthropic → assignments.chat 指向 anthropic 渠道 + sonnet
        let (chan, model) = cfg.resolve_pipeline(PipelineKind::Chat).unwrap();
        assert_eq!(chan.name, "Anthropic");
        assert_eq!(model, "claude-sonnet-4-6");
        assert_eq!(chan.thinking_mode, ThinkingMode::Enabled);
        assert_eq!(chan.thinking_budget_tokens, 6000);
    }

    #[test]
    fn migrate_with_legacy_openai_field_duplicates_to_responses_and_chat() {
        // 最早期的 openai 单字段——迁移后 openaiResponses + openaiChatCompletions 都拿到
        let v = json!({
            "provider": "openai_chat_completions",
            "anthropic": {"baseUrl":"","token":""},
            "openai": {
                "baseUrl": "https://api.deepseek.com",
                "token": "sk-x",
                "availableModels": ["deepseek-chat"],
                "models": {"chat":"deepseek-chat","briefing":"","review":"","compact":""}
            },
            "agent": {"maxTurnsPerRun":12,"maxSearchCallsPerRun":5,"contextSoftLimitTokens":80000,"contextHardLimitTokens":160000,"compactKeepLastNTurns":6,"toolTimeoutSecs":30}
        });
        let cfg = parse_with_migration(v);
        assert_eq!(cfg.channels.len(), 3); // anthropic + responses + chat_completions
        let (chan, model) = cfg.resolve_pipeline(PipelineKind::Chat).unwrap();
        assert_eq!(chan.wire_format, ProviderKind::OpenAIChatCompletions);
        assert_eq!(model, "deepseek-chat");
    }

    #[test]
    fn channel_token_masking_round_trip() {
        let masked = mask_token("sk-abcdefghij");
        assert!(masked.starts_with("sk-abcde"));
        assert!(masked.contains("(13 chars)"));
    }
}
