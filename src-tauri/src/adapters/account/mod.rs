//! Account BC adapters — Tauri commands + 事件常量 + DTO 包装。
//!
//! Spec: docs/design/account-module.md §4 (对外接口)

pub mod cmd;
pub mod events;

pub use cmd::{
    fetch_account, mark_trigger_handled, operate_account, rebuild_account_snapshot,
    update_watchlist,
};
pub use events::{
    wrap_account_triggered, wrap_account_updated, AccountTriggeredPayload,
    AccountUpdatedPayload, ACCOUNT_TRIGGERED_EVENT, ACCOUNT_UPDATED_EVENT,
};
