//! Agent Infra Tauri commands —— 提供只读 introspection 入口。
//!
//! Spec: docs/design/agent-infra-module.md §5（对外接口）
//!
//! 本 Phase 暴露的 commands：
//! - `agent_list_tools`：列出当前 ToolRegistry 注册的所有 ToolSpec（前端调试 / Runtime 自检用）。
//!
//! 注：触发 Agent run / 提交用户消息的入口由 Runtime 拥有（spec §5 末段）。

use crate::adapters::error::CommandError;
use crate::domain::agent::{ProviderChannel, ToolSpec, WireFormat};
use crate::domain::shared::ErrorCode;
use crate::infrastructure::agent::{
    channel_presets, discover_models, AgentInfra, DiscoveredModel,
};
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State;

/// 列出当前 ToolRegistry 已注册的 tool。
///
/// Spec: agent-infra-module.md §5 Tool Registry API（snapshot）
#[tauri::command]
#[specta::specta]
pub fn agent_list_tools(
    infra: State<'_, AgentInfra>,
) -> Result<Vec<ToolSpec>, CommandError> {
    Ok(infra.registry.list_tools())
}

// ===========================================================================
// ProviderChannel 配置 + 模型发现（设置页）
// Spec: agent-infra-module.md §2 `ProviderChannel`，§5 前端命令（设置页）
// ===========================================================================

/// 快速预设视图（返回给设置页）。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ChannelPresetView {
    pub key: String,
    pub provider: String,
    pub wire_format: WireFormat,
    pub base_url: String,
}

/// 渠道视图 —— **屏蔽 apiKey 明文**，只回 `apiKeySet: bool`。
///
/// Spec §2：apiKey 只写不读；list / get DTO 必须屏蔽明文。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ProviderChannelView {
    pub channel_id: String,
    pub provider: String,
    pub wire_format: WireFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    pub model: String,
    pub enabled: bool,
    pub is_active: bool,
    /// apiKey 是否已设置（永远不回传明文）。
    pub api_key_set: bool,
}

/// 添加渠道的请求 —— 一条确认保留的模型成一条渠道。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AddChannelInput {
    /// 用户输入的渠道名（展示用 provider name）。
    pub provider: String,
    pub wire_format: WireFormat,
    #[serde(default)]
    pub base_url: Option<String>,
    pub api_key: String,
    pub model: String,
    /// 可选能力标记；缺省 false。
    #[serde(default)]
    pub supports_vision: Option<bool>,
    #[serde(default)]
    pub supports_thinking: Option<bool>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub context_window_tokens: Option<u32>,
}

/// 编辑已有渠道的请求 —— 按 channelId 更新可编辑字段。
///
/// Spec §5 前端命令：`agent_update_channel`。apiKey 为空/省略 = 保留原 key 不变；
/// 不修改 is_active；未暴露的能力字段（thinking budget 等）保留原值。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct UpdateChannelInput {
    pub channel_id: String,
    pub provider: String,
    pub wire_format: WireFormat,
    #[serde(default)]
    pub base_url: Option<String>,
    pub model: String,
    /// 空/省略 = 保留原 key；非空才覆盖。
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub supports_vision: Option<bool>,
    #[serde(default)]
    pub supports_thinking: Option<bool>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub context_window_tokens: Option<u32>,
}

fn map_repo_err(e: impl std::fmt::Display) -> CommandError {
    CommandError::with_message(ErrorCode::DbError, e.to_string())
}

/// 返回内置快速预设（DeepSeek / OpenAI / Anthropic 官方）。
///
/// Spec §5 前端命令：`agent_channel_presets`
#[tauri::command]
#[specta::specta]
pub fn agent_channel_presets() -> Result<Vec<ChannelPresetView>, CommandError> {
    Ok(channel_presets()
        .into_iter()
        .map(|p| ChannelPresetView {
            key: p.key.to_string(),
            provider: p.provider.to_string(),
            wire_format: p.wire_format,
            base_url: p.base_url.to_string(),
        })
        .collect())
}

/// 调对应 wireFormat 的 `/models` 接口发现可用模型。
///
/// Spec §5 模型发现 API：发现失败时调用方引导用户手填模型名。
#[tauri::command]
#[specta::specta]
pub async fn agent_discover_models(
    wire_format: WireFormat,
    base_url: String,
    api_key: String,
) -> Result<Vec<DiscoveredModel>, CommandError> {
    discover_models(wire_format, &base_url, &api_key)
        .await
        .map_err(|e| CommandError::with_message(ErrorCode::ProviderUnavailable, e.to_string()))
}

/// 添加一条渠道。生成 channelId（`ch_<uuid>`），stream=true，能力缺省 false。
/// 若是首条渠道，自动设为 active。
///
/// Spec §2：每个确认保留的模型各成一条 ProviderChannel；维护 active 渠道。
#[tauri::command]
#[specta::specta]
pub fn agent_add_channel(
    infra: State<'_, AgentInfra>,
    input: AddChannelInput,
) -> Result<(), CommandError> {
    let channel_id = format!("ch_{}", uuid::Uuid::new_v4());
    let channel = ProviderChannel {
        channel_id: channel_id.clone(),
        provider: input.provider,
        wire_format: input.wire_format,
        base_url: input.base_url,
        api_key: input.api_key,
        model: input.model,
        stream: true,
        enabled: true,
        supports_vision: input.supports_vision.unwrap_or(false),
        supports_thinking: input.supports_thinking.unwrap_or(false),
        max_output_tokens: input.max_output_tokens,
        context_window_tokens: input.context_window_tokens,
        // FIX 4: thinking budget not exposed via add-channel DTO yet; default off.
        thinking_budget_tokens: None,
    };

    let was_empty = infra.channels_repo.list().map_err(map_repo_err)?.is_empty();
    infra.channels_repo.add(&channel).map_err(map_repo_err)?;
    if was_empty {
        infra
            .channels_repo
            .set_active(&channel_id)
            .map_err(map_repo_err)?;
    }
    Ok(())
}

/// 编辑已有渠道（按 channelId）。apiKey 为空/省略保留原 key；不改 is_active；
/// 未暴露字段保留原值。channelId 不存在 → not_found。
///
/// Spec §5 前端命令：`agent_update_channel`
#[tauri::command]
#[specta::specta]
pub fn agent_update_channel(
    infra: State<'_, AgentInfra>,
    input: UpdateChannelInput,
) -> Result<(), CommandError> {
    let existing = infra
        .channels_repo
        .get(&input.channel_id)
        .map_err(map_repo_err)?
        .ok_or_else(|| {
            CommandError::with_message(
                ErrorCode::NotFound,
                format!("channel not found: {}", input.channel_id),
            )
        })?;

    // apiKey 语义：空/省略 = 保留原 key；非空才覆盖。
    let api_key = match input.api_key {
        Some(k) if !k.trim().is_empty() => k,
        _ => existing.api_key,
    };

    let channel = ProviderChannel {
        channel_id: existing.channel_id,
        provider: input.provider,
        wire_format: input.wire_format,
        base_url: input.base_url,
        api_key,
        model: input.model,
        stream: true,
        enabled: input.enabled.unwrap_or(existing.enabled),
        supports_vision: input.supports_vision.unwrap_or(existing.supports_vision),
        supports_thinking: input
            .supports_thinking
            .unwrap_or(existing.supports_thinking),
        max_output_tokens: input.max_output_tokens.or(existing.max_output_tokens),
        context_window_tokens: input
            .context_window_tokens
            .or(existing.context_window_tokens),
        // 未在 input 暴露：保留原值。
        thinking_budget_tokens: existing.thinking_budget_tokens,
    };

    infra.channels_repo.update(&channel).map_err(map_repo_err)
}

/// 列出所有渠道（**屏蔽 apiKey 明文**）。
///
/// Spec §5 前端命令：`agent_list_channels`（屏蔽 apiKey）
#[tauri::command]
#[specta::specta]
pub fn agent_list_channels(
    infra: State<'_, AgentInfra>,
) -> Result<Vec<ProviderChannelView>, CommandError> {
    let active_id = infra
        .channels_repo
        .active()
        .map_err(map_repo_err)?
        .map(|c| c.channel_id);
    let channels = infra.channels_repo.list().map_err(map_repo_err)?;
    Ok(channels
        .into_iter()
        .map(|c| to_view(&c, active_id.as_deref()))
        .collect())
}

/// 删除一条渠道。
#[tauri::command]
#[specta::specta]
pub fn agent_remove_channel(
    infra: State<'_, AgentInfra>,
    channel_id: String,
) -> Result<(), CommandError> {
    infra
        .channels_repo
        .remove(&channel_id)
        .map_err(map_repo_err)
}

/// 设置当前 active 渠道。
///
/// Spec §5 前端命令：`agent_set_active_channel`
#[tauri::command]
#[specta::specta]
pub fn agent_set_active_channel(
    infra: State<'_, AgentInfra>,
    channel_id: String,
) -> Result<(), CommandError> {
    infra
        .channels_repo
        .set_active(&channel_id)
        .map_err(map_repo_err)
}

/// 读取当前 active 渠道（屏蔽 apiKey）。
#[tauri::command]
#[specta::specta]
pub fn agent_get_active_channel(
    infra: State<'_, AgentInfra>,
) -> Result<Option<ProviderChannelView>, CommandError> {
    let active = infra.channels_repo.active().map_err(map_repo_err)?;
    Ok(active.map(|c| {
        let id = c.channel_id.clone();
        to_view(&c, Some(&id))
    }))
}

/// 把 domain channel 映射成屏蔽 apiKey 的视图。
fn to_view(c: &ProviderChannel, active_id: Option<&str>) -> ProviderChannelView {
    ProviderChannelView {
        channel_id: c.channel_id.clone(),
        provider: c.provider.clone(),
        wire_format: c.wire_format,
        base_url: c.base_url.clone(),
        model: c.model.clone(),
        enabled: c.enabled,
        is_active: active_id == Some(c.channel_id.as_str()),
        api_key_set: !c.api_key.is_empty(),
    }
}
