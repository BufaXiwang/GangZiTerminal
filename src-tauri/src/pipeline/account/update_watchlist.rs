//! `update_watchlist` 用例 —— spec `account-module.md §4 update_watchlist`。
//!
//! 写动作：
//! 1. 校验 tsCode 是否 Quotes universe 已知（spec：未知返回 not_found）
//! 2. 幂等处理（重复 add / 不存在 remove 不重复写事件）
//! 3. append watchlist_added / watchlist_removed / watchlist_note_updated
//! 4. 更新内存 watchlist + KV
//! 5. 返回 accountEventIds + 最新 item

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::domain::account::account_event::{AccountEvent, AccountEventType};
use crate::domain::account::events::AccountActor;
use crate::domain::shared::{ErrorCode, StockCode};
use crate::infrastructure::account::{account_events_repo, watchlist, watchlist_events};
use crate::pipeline::quotes_universe;

fn parse_actor(s: &str) -> AccountActor {
    match s {
        "agent" => AccountActor::Agent,
        "system" => AccountActor::System,
        _ => AccountActor::User,
    }
}

/// 把 watchlist 事件镜像到统一 `account_events` 流 —— spec `account-module.md §2`
/// 「所有账户状态变化必须先 append AccountEvent」。本地 watchlist_events 表保留
/// 给读模型（note_for / 派生 addedAt 用），但 spec contract 的真源是 account_events。
fn mirror_to_account_events(
    app: &tauri::AppHandle,
    event_type: AccountEventType,
    actor: &str,
    ts_code: &str,
    note: Option<&str>,
    reason: Option<&str>,
) -> Option<String> {
    let event = AccountEvent::new(
        event_type,
        parse_actor(actor),
        serde_json::json!({ "note": note }),
    )
    .with_ts_code(ts_code)
    .with_reason(reason.unwrap_or(""));
    match account_events_repo::append(app, &event) {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::warn!(
                target = "account.update_watchlist",
                error = %e,
                event_type = event_type.as_str(),
                "镜像 AccountEvent 失败"
            );
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchlistAction {
    Add,
    Remove,
    UpdateNote,
}

#[derive(Debug, Clone)]
pub struct UpdateWatchlistInput {
    pub action: WatchlistAction,
    pub ts_code: String,
    pub note: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchlistItemView {
    pub ts_code: String,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateWatchlistResponse {
    pub accepted: bool,
    /// spec `shared-types.md §5` ErrorCode 闭集合，不接受自由字符串。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<ErrorCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<WatchlistItemView>,
    #[serde(default)]
    pub account_event_ids: Vec<String>,
}

impl UpdateWatchlistResponse {
    pub fn rejected(reason: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            accepted: false,
            reason: Some(reason),
            message: Some(message.into()),
            item: None,
            account_event_ids: Vec::new(),
        }
    }
}

/// 执行 update_watchlist。
///
/// - `actor`：spec `AccountActor`（user / agent / system）。
pub fn dispatch(
    app: &AppHandle,
    actor: &str,
    input: UpdateWatchlistInput,
) -> UpdateWatchlistResponse {
    let ts_code_raw = input.ts_code.trim().to_uppercase();
    if ts_code_raw.is_empty() {
        return UpdateWatchlistResponse::rejected(ErrorCode::InvalidInput, "tsCode 不能为空");
    }
    // spec §4：未知 tsCode 返回 not_found；股票 / 指数 / 基金都允许加入观察列表
    if !quotes_universe::is_known_instrument(app, &ts_code_raw) {
        return UpdateWatchlistResponse::rejected(
            ErrorCode::NotFound,
            format!("Quotes universe 未知标的 `{ts_code_raw}`"),
        );
    }
    let code = StockCode::new_unchecked(ts_code_raw.clone());

    match input.action {
        WatchlistAction::Add => {
            let already = watchlist::contains(&code);
            // spec §4：重复 add 是幂等更新；只在首次或 note 变化时写事件
            let current_note = watchlist_events::note_for(app, &ts_code_raw).ok().flatten();
            let note_changed = match (&input.note, &current_note) {
                (Some(n), Some(c)) => n != c,
                (Some(_), None) => true,
                _ => false,
            };
            if already && !note_changed {
                return UpdateWatchlistResponse {
                    accepted: true,
                    reason: None,
                    message: Some("idempotent_no_change".into()),
                    item: Some(WatchlistItemView {
                        ts_code: ts_code_raw,
                        note: current_note,
                    }),
                    account_event_ids: Vec::new(),
                };
            }
            watchlist::add(app, code);
            let event_id = match watchlist_events::append(
                app,
                watchlist_events::WatchlistEventType::Added,
                actor,
                &ts_code_raw,
                input.note.as_deref(),
                input.reason.as_deref(),
            ) {
                Ok(id) => id,
                Err(msg) => return UpdateWatchlistResponse::rejected(ErrorCode::DbError, msg),
            };
            let mut event_ids = vec![event_id];
            if let Some(acc_id) = mirror_to_account_events(
                app,
                AccountEventType::WatchlistAdded,
                actor,
                &ts_code_raw,
                input.note.as_deref(),
                input.reason.as_deref(),
            ) {
                event_ids.push(acc_id);
            }
            UpdateWatchlistResponse {
                accepted: true,
                reason: None,
                message: None,
                item: Some(WatchlistItemView {
                    ts_code: ts_code_raw,
                    note: input.note,
                }),
                account_event_ids: event_ids,
            }
        }
        WatchlistAction::Remove => {
            let present = watchlist::contains(&code);
            if !present {
                // spec §4：remove 不存在自选项幂等返回 accepted=true 不写事件
                return UpdateWatchlistResponse {
                    accepted: true,
                    reason: None,
                    message: Some("idempotent_not_present".into()),
                    item: None,
                    account_event_ids: Vec::new(),
                };
            }
            watchlist::remove(app, &code);
            let event_id = match watchlist_events::append(
                app,
                watchlist_events::WatchlistEventType::Removed,
                actor,
                &ts_code_raw,
                None,
                input.reason.as_deref(),
            ) {
                Ok(id) => id,
                Err(msg) => return UpdateWatchlistResponse::rejected(ErrorCode::DbError, msg),
            };
            let mut event_ids = vec![event_id];
            if let Some(acc_id) = mirror_to_account_events(
                app,
                AccountEventType::WatchlistRemoved,
                actor,
                &ts_code_raw,
                None,
                input.reason.as_deref(),
            ) {
                event_ids.push(acc_id);
            }
            UpdateWatchlistResponse {
                accepted: true,
                reason: None,
                message: None,
                item: None,
                account_event_ids: event_ids,
            }
        }
        WatchlistAction::UpdateNote => {
            if !watchlist::contains(&code) {
                return UpdateWatchlistResponse::rejected(
                    ErrorCode::NotFound,
                    format!("{ts_code_raw} 不在自选；先调 add"),
                );
            }
            let event_id = match watchlist_events::append(
                app,
                watchlist_events::WatchlistEventType::NoteUpdated,
                actor,
                &ts_code_raw,
                input.note.as_deref(),
                input.reason.as_deref(),
            ) {
                Ok(id) => id,
                Err(msg) => return UpdateWatchlistResponse::rejected(ErrorCode::DbError, msg),
            };
            let mut event_ids = vec![event_id];
            if let Some(acc_id) = mirror_to_account_events(
                app,
                AccountEventType::WatchlistNoteUpdated,
                actor,
                &ts_code_raw,
                input.note.as_deref(),
                input.reason.as_deref(),
            ) {
                event_ids.push(acc_id);
            }
            UpdateWatchlistResponse {
                accepted: true,
                reason: None,
                message: None,
                item: Some(WatchlistItemView {
                    ts_code: ts_code_raw,
                    note: input.note,
                }),
                account_event_ids: event_ids,
            }
        }
    }
}

pub fn parse_action(s: &str) -> Option<WatchlistAction> {
    Some(match s {
        "add" => WatchlistAction::Add,
        "remove" => WatchlistAction::Remove,
        "update_note" => WatchlistAction::UpdateNote,
        _ => return None,
    })
}
