//! StrategyCard Tauri commands —— spec `agent-runtime-module.md §9`。
//!
//! - `fetch_strategy_cards`：列表
//! - `upsert_strategy_card`：显式创建 / 调整（agent suggestedChange 不自动调用）

use crate::infrastructure::agent_runtime::{strategy_audit_repo, strategy_cards_repo};
use crate::infrastructure::db::helpers::now;
use crate::domain::agent_runtime::decisions::{
    StrategyCard, StrategyCardConfig, StrategyStatus,
};
use serde::{Deserialize, Serialize};
use tauri::AppHandle;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchStrategyCardsRequest {
    pub status: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchStrategyCardsResponse {
    pub items: Vec<StrategyCard>,
}

#[tauri::command]
pub fn fetch_strategy_cards(
    app: AppHandle,
    request: Option<FetchStrategyCardsRequest>,
) -> Result<FetchStrategyCardsResponse, String> {
    let filter = request.and_then(|r| r.status);
    let items = strategy_cards_repo::list(&app, filter.as_deref())?;
    Ok(FetchStrategyCardsResponse { items })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpsertStrategyCardRequest {
    pub strategy_id: Option<String>,
    pub base_version: Option<u32>,
    pub name: String,
    pub description: String,
    pub status: String,
    pub config: StrategyCardConfig,
    pub reason: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpsertStrategyCardResponse {
    pub accepted: bool,
    pub strategy_id: String,
    pub version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[tauri::command]
pub fn upsert_strategy_card(
    app: AppHandle,
    request: UpsertStrategyCardRequest,
) -> Result<UpsertStrategyCardResponse, String> {
    let status = match request.status.as_str() {
        "active" => StrategyStatus::Active,
        "paused" => StrategyStatus::Paused,
        other => return Err(format!("invalid status: {other}")),
    };
    let strategy_id = request
        .strategy_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // 乐观并发：拿当前 version，与 base_version 对比。
    // spec §9：baseVersion 用于乐观并发——已存在但调用方未传 baseVersion 必须拒绝。
    let existing = strategy_cards_repo::list(&app, None)?
        .into_iter()
        .find(|c| c.strategy_id == strategy_id);
    let (created_at, new_version) = match (&existing, request.base_version) {
        (Some(card), None) => {
            return Ok(UpsertStrategyCardResponse {
                accepted: false,
                strategy_id,
                version: card.version,
                reason: Some("version_conflict".into()),
            });
        }
        (Some(card), Some(base)) if card.version != base => {
            return Ok(UpsertStrategyCardResponse {
                accepted: false,
                strategy_id,
                version: card.version,
                reason: Some("version_conflict".into()),
            });
        }
        (Some(card), _) => (card.created_at.clone(), card.version + 1),
        (None, _) => (now(), 1),
    };
    let card = StrategyCard {
        strategy_id: strategy_id.clone(),
        version: new_version,
        name: request.name,
        description: request.description,
        status,
        config: request.config,
        created_at,
        updated_at: now(),
    };
    strategy_cards_repo::upsert(&app, &card)?;
    strategy_audit_repo::append(&app, &card, &request.reason)?;
    Ok(UpsertStrategyCardResponse {
        accepted: true,
        strategy_id,
        version: new_version,
        reason: None,
    })
}
