//! Tauri IPC——模拟账户的**只读 + watchlist + reset** API。
//!
//! 写交易（open/close/scale/adjust）**不**通过 IPC 暴露——只有 agent tool 内部能调
//! `pipeline::account::AccountService`。前端永远没有"开仓"按钮。
//!
//! Watchlist 的 add 双方都能调（用户 UI + agent tool）；remove 仅用户。

use crate::domain::account::types::{AccountSnapshot, Position, PositionEvent, PositionId};
use crate::domain::shared::StockCode;
use crate::infrastructure::account::{snapshot_cache, watchlist, PositionRepo};
use crate::pipeline::account::AccountService;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use tauri::{AppHandle, Emitter};

const EVENT_WATCHLIST_CHANGED: &str = "watchlist-changed";

// ============================================================================
// AccountSnapshot 读取
// ============================================================================

/// 当前账户快照——从 ACCOUNT_SNAPSHOT in-memory cache 读。
/// 启动初期一两秒内可能未填充，返 None 让前端显示 loading。
#[tauri::command]
pub fn get_account_snapshot() -> Option<AccountSnapshot> {
    snapshot_cache::get()
}

// ============================================================================
// Positions 读取
// ============================================================================

#[tauri::command]
pub fn list_positions(app: AppHandle) -> Result<Vec<Position>, String> {
    PositionRepo::new(app).list_all().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn list_position_events(
    app: AppHandle,
    position_id: String,
) -> Result<Vec<PositionEvent>, String> {
    let id = PositionId::from_string(position_id);
    PositionRepo::new(app)
        .list_events(&id)
        .map_err(|e| e.to_string())
}

// ============================================================================
// Watchlist CRUD
// ============================================================================

/// 自选股 + 行情元信息（name / sector / category）—— 前端"历史行情"列表用。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchlistEntry {
    pub ts_code: String,
    pub code: String,
    pub name: String,
}

#[tauri::command]
pub fn list_watchlist() -> Vec<String> {
    watchlist::list()
        .into_iter()
        .map(|c| c.as_str().to_string())
        .collect()
}

/// 带元信息的自选股——前端"历史行情"列表展示用。
#[tauri::command]
pub fn list_watchlist_with_info(app: AppHandle) -> Vec<WatchlistEntry> {
    let stock_map: HashMap<String, (String, String)> = crate::infrastructure::quotes::repository::list_stocks(&app)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| {
            let suffix = match row.market.as_str() {
                "sh" => "SH",
                "sz" => "SZ",
                "bj" => "BJ",
                _ => return None,
            };
            Some((
                row.code.clone(),
                (format!("{}.{}", row.code, suffix), row.name),
            ))
        })
        .collect();

    watchlist::list()
        .into_iter()
        .filter_map(|c| {
            let code_str = c.as_str().to_string();
            let (ts_code, name) = stock_map.get(&code_str)?.clone();
            Some(WatchlistEntry {
                ts_code,
                code: code_str,
                name,
            })
        })
        .collect()
}

#[tauri::command]
pub fn add_watchlist_code(app: AppHandle, code: String) -> Result<Vec<String>, String> {
    let parsed = StockCode::new(&code).map_err(|e| e.to_string())?;
    let code = parsed.as_str().to_string();
    watchlist::add(&app, parsed);
    let list = list_watchlist();
    emit_watchlist_changed(&app, "add", &code, list.len());
    spawn_immediate_quote_refresh(&app);
    Ok(list)
}

#[tauri::command]
pub fn remove_watchlist_code(app: AppHandle, code: String) -> Result<Vec<String>, String> {
    let parsed = StockCode::new(&code).map_err(|e| e.to_string())?;
    let code = parsed.as_str().to_string();
    watchlist::remove(&app, &parsed);
    let list = list_watchlist();
    emit_watchlist_changed(&app, "remove", &code, list.len());
    Ok(list)
}

fn emit_watchlist_changed(app: &AppHandle, action: &str, code: &str, total: usize) {
    let _ = app.emit(
        EVENT_WATCHLIST_CHANGED,
        json!({
            "action": action,
            "code": code,
            "total": total,
            "capturedAt": chrono::Utc::now().to_rfc3339(),
        }),
    );
}

fn spawn_immediate_quote_refresh(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match crate::pipeline::market_refresh::run_market_quote_refresh(&app).await {
            Ok(summary) => tracing::info!(
                total = summary.total,
                success = summary.success,
                "watchlist 变更后订阅集行情即时刷新完成"
            ),
            Err(e) => tracing::warn!(error = %e, "watchlist 变更后订阅集行情即时刷新失败"),
        }
    });
}

/// 默认自选股——首次启动时前端 hydrate 用。
#[tauri::command]
pub fn get_default_watchlist() -> Vec<String> {
    vec!["000001", "399001", "600519", "300750"]
        .into_iter()
        .map(String::from)
        .collect()
}

// ============================================================================
// 一键重置
// ============================================================================

#[tauri::command]
pub async fn reset_simulation_account(app: AppHandle) -> Result<usize, String> {
    let service = AccountService::new(app);
    service.reset().await.map_err(|e| e.to_string())
}
