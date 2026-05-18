//! Chat pipeline 用的跨 aggregate 上下文读取。
//!
//! - `read_positions` / `read_position_events_for_open`：从 `PositionRepo` 拿活仓 + 事件链
//! - `collect_relevant_codes`：watchlist + open positions 去重，作为行情拉取范围

use crate::domain::account::types::{Position, PositionEvent, PositionId};
use crate::infrastructure::account::PositionRepo;
use std::collections::{HashMap, HashSet};
use tauri::AppHandle;

/// 读所有 open 持仓——走 domain repo（含 status==Open 过滤）。
pub fn read_positions(app: &AppHandle) -> Result<Vec<Position>, String> {
    let repo = PositionRepo::new(app.clone());
    repo.list_open().map_err(|e| e.to_string())
}

/// 对 open 持仓查事件链——key 是 PositionId.as_str()，方便 prompt formatter 索引。
pub fn read_position_events_for_open(
    app: &AppHandle,
    positions: &[Position],
) -> HashMap<String, Vec<PositionEvent>> {
    if positions.is_empty() {
        return HashMap::new();
    }
    let repo = PositionRepo::new(app.clone());
    let ids: Vec<PositionId> = positions.iter().map(|p| p.id.clone()).collect();
    let events = match repo.list_events_batch(&ids) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };
    let mut map: HashMap<String, Vec<PositionEvent>> = HashMap::new();
    for event in events {
        map.entry(event.position_id.as_str().to_string())
            .or_default()
            .push(event);
    }
    map
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
