//! 实时报价代理池 IPC——Settings → 网络 tab 调用。
//!
//! - `get_proxy_pool` 返当前 proxy 列表 + 每条的健康度
//! - `set_proxy_pool` 替换列表（同时持久化）
//! - `get_realtime_health` 返三源 EMA 健康度（observability）

use crate::infrastructure::quotes::realtime::{dispatch, proxy_pool};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyPoolDto {
    pub urls: Vec<String>,
    pub health: Vec<proxy_pool::ProxyHealthSnapshot>,
}

#[tauri::command]
pub fn get_proxy_pool() -> ProxyPoolDto {
    ProxyPoolDto {
        urls: proxy_pool::pool().list_urls(),
        health: proxy_pool::pool().snapshot(),
    }
}

#[derive(Debug, Deserialize)]
pub struct SetProxyPoolArgs {
    pub urls: Vec<String>,
}

#[tauri::command]
pub fn set_proxy_pool(app: tauri::AppHandle, args: SetProxyPoolArgs) -> Result<(), String> {
    let cleaned: Vec<String> = args
        .urls
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    // 入池——立刻生效（client cache 仍会被旧 proxy URL 引用到，下一轮 dispatch 自动按新 list 走）
    proxy_pool::pool().set_urls(cleaned.clone());
    // 持久化
    proxy_pool::persist(&app, &cleaned)?;
    tracing::info!(count = cleaned.len(), "代理池更新");
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceHealthDto {
    pub name: String,
    pub health: f64,
}

#[tauri::command]
pub fn get_realtime_health() -> Vec<SourceHealthDto> {
    dispatch()
        .health_snapshot()
        .into_iter()
        .map(|(name, health)| SourceHealthDto {
            name: name.to_string(),
            health,
        })
        .collect()
}
