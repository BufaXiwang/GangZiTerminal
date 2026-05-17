//! Agent Tauri IPC commands——前端 SettingsPage 入口。
//!
//! 三个命令：
//! - `get_agent_config`：读 app_state["agent.config"]，token 字段 mask 后返
//! - `set_agent_config`：写回；未编辑的 token（空 / mask 形态）保留旧值
//! - `verify_provider_model`：发一条 1-token 探针验证渠道 + 模型可用
//!
//! 业务逻辑（read/write_agent_config / build_provider_for_channel / AgentConfig
//! 结构）在 `pipeline::agent::config`，这里只做 IPC 边界专属处理：token mask、
//! 未编辑回退、参数命名（camelCase）等。

use crate::infrastructure::agent::provider::models::verify_model;
use crate::pipeline::agent::config::{read_agent_config, write_agent_config, AgentConfig};
use serde_json::Value;
use tauri::AppHandle;

#[tauri::command]
pub fn get_agent_config(app: AppHandle) -> Value {
    let cfg = read_agent_config(&app);
    let mut value = serde_json::to_value(&cfg).unwrap_or(Value::Null);
    // 每个渠道的 token 都 mask 掉——避免在 IPC payload 中以明文回传到前端
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

/// 校验某个 model id 在某个渠道下是否能跑通——发 1-token 探针。
///
/// `baseUrl` / `token` 接受 mask 形态或空，自动回退到 stored 渠道里的值
/// （前端"还没保存就想 verify"的情况）。
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
            return Err(format!(
                "未找到 channel id={channelId}——请先保存渠道后再验证"
            ));
        }
    };
    if resolved_url.trim().is_empty() || resolved_token.trim().is_empty() {
        return Err("base_url 或 token 为空".into());
    }
    verify_model(wire_format, &resolved_url, &resolved_token, &model)
        .await
        .map_err(|e| format!("{e}"))
}

// ===== IPC 专属 helpers ===================================================

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_token_masking_round_trip() {
        let masked = mask_token("sk-abcdefghij");
        assert!(masked.starts_with("sk-abcde"));
        assert!(masked.contains("(13 chars)"));
    }
}
