//! Account Tauri commands — fetch_account / update_watchlist /
//! mark_trigger_handled / rebuild_account_snapshot。
//!
//! Spec: docs/design/account-module.md §4
//!
//! 注意：spec §4 — `operate_account` 写入口只对 Agent tool / 外部自动化决策运行时暴露，
//! 不通过 Tauri command 注册供前端 invoke。`operate_account` 函数本身保留（带
//! `#[allow(dead_code)]`），将在 Phase 3 由 Agent Runtime 通过 SkillRegistry
//! 注册为 skill；写路径必须经过 Agent 决策链，不接受前端 UI 直发。

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

/// Account 写入口。
///
/// Spec: account-module.md §4 — 不通过 Tauri command 暴露；将由 Phase 3 Agent
/// Runtime 通过 SkillRegistry 注册为 skill。保留函数签名以供后续注册和当前
/// `service.operate_account` 的 adapter 风格统一。
#[allow(dead_code)]
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
