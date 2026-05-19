//! Chat pipeline 用的跨 aggregate 上下文读取。
//!
//! - `read_positions`：从 `PositionRepo` 拿活仓
//! - `collect_relevant_codes`：watchlist + open positions 去重，作为行情拉取范围

use crate::domain::account::types::Position;
use crate::infrastructure::account::PositionRepo;
use std::collections::HashSet;
use tauri::AppHandle;

/// 读所有 open 持仓——走 domain repo（含 status==Open 过滤）。
pub fn read_positions(app: &AppHandle) -> Result<Vec<Position>, String> {
    let repo = PositionRepo::new(app.clone());
    repo.list_open().map_err(|e| e.to_string())
}

/// 把 watchlist + open positions 的 code 合并，去重后返回。
pub fn collect_relevant_codes(watchlist: &[String], positions: &[Position]) -> Vec<String> {
    let mut set: HashSet<String> = watchlist.iter().cloned().collect();
    for p in positions {
        if p.status.is_open() {
            set.insert(p.code.as_str().to_string());
        }
    }
    set.into_iter().collect()
}
