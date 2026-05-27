//! Account Tauri commands — fetch_account / operate_account / update_watchlist /
//! mark_trigger_handled / rebuild_account_snapshot。
//!
//! Spec: docs/design/account-module.md §4
//!
//! 注意：spec §4 — `operate_account` 是 Agent tool / 自动化决策入口，前端不应通过它创建订单；
//! 这里仍然导出为 IPC command（开发 / 调试可用），但 actor 强制为 `agent`，并要求显式 reason。

use crate::adapters::error::CommandError;
use crate::domain::account::requests::{
    AccountActor, FetchAccountRequest, FetchAccountResponse, MarkTriggerHandledRequest,
    MarkTriggerHandledResponse, OperateAccountRequest, OperateAccountResponse,
    UpdateWatchlistRequest, UpdateWatchlistResponse,
};
use crate::domain::account::types::AccountSnapshot;
use crate::domain::shared::ErrorCode;
use crate::pipeline::account::AccountService;
use std::sync::Arc;
use tauri::State;

#[tauri::command]
#[specta::specta]
pub fn fetch_account(
    request: FetchAccountRequest,
    service: State<'_, Arc<AccountService>>,
) -> Result<FetchAccountResponse, CommandError> {
    Ok(service.fetch_account(request))
}

#[tauri::command]
#[specta::specta]
pub fn operate_account(
    request: OperateAccountRequest,
    service: State<'_, Arc<AccountService>>,
) -> Result<OperateAccountResponse, CommandError> {
    Ok(service.operate_account(request, AccountActor::Agent))
}

#[tauri::command]
#[specta::specta]
pub fn update_watchlist(
    request: UpdateWatchlistRequest,
    service: State<'_, Arc<AccountService>>,
) -> Result<UpdateWatchlistResponse, CommandError> {
    // Watchlist 允许 user actor。前端默认走 user。
    Ok(service.update_watchlist(request, AccountActor::User))
}

#[tauri::command]
#[specta::specta]
pub fn mark_trigger_handled(
    request: MarkTriggerHandledRequest,
    service: State<'_, Arc<AccountService>>,
) -> Result<MarkTriggerHandledResponse, CommandError> {
    Ok(service.mark_trigger_handled(request))
}

#[tauri::command]
#[specta::specta]
pub fn rebuild_account_snapshot(
    service: State<'_, Arc<AccountService>>,
) -> Result<AccountSnapshot, CommandError> {
    service
        .rebuild_account_snapshot()
        .map_err(|code: ErrorCode| CommandError::new(code))
}
