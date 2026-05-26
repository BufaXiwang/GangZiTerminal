//! Watchlist 事件持久化 + note 读模型 —— spec `account-module.md §2 watchlist_*`。
//!
//! 写动作生成事件 + 必要时更新 watchlist_notes 读模型。

use rusqlite::params;
use tauri::AppHandle;

use crate::infrastructure::db::helpers::now;
use crate::infrastructure::db::{migrate, open_database};

#[derive(Debug, Clone, Copy)]
pub enum WatchlistEventType {
    Added,
    Removed,
    NoteUpdated,
}

impl WatchlistEventType {
    pub fn as_str(self) -> &'static str {
        match self {
            WatchlistEventType::Added => "watchlist_added",
            WatchlistEventType::Removed => "watchlist_removed",
            WatchlistEventType::NoteUpdated => "watchlist_note_updated",
        }
    }
}

/// 写一条 watchlist 事件，返回 event_id。
pub fn append(
    app: &AppHandle,
    event_type: WatchlistEventType,
    actor: &str,
    ts_code: &str,
    note: Option<&str>,
    reason: Option<&str>,
) -> Result<String, String> {
    let c = open_database(app)?;
    migrate(&c)?;
    let event_id = uuid::Uuid::new_v4().to_string();
    c.execute(
        "insert into watchlist_events(event_id, event_type, actor, ts_code, note, reason, occurred_at)
         values (?1,?2,?3,?4,?5,?6,?7)",
        params![event_id, event_type.as_str(), actor, ts_code, note, reason, now()],
    )
    .map_err(|e| format!("写 watchlist_event 失败：{e}"))?;
    match event_type {
        WatchlistEventType::Added | WatchlistEventType::NoteUpdated => {
            if let Some(n) = note {
                c.execute(
                    "insert into watchlist_notes(ts_code, note, updated_at) values (?1,?2,?3)
                     on conflict(ts_code) do update set note = excluded.note, updated_at = excluded.updated_at",
                    params![ts_code, n, now()],
                )
                .map_err(|e| format!("写 watchlist_notes 失败：{e}"))?;
            }
        }
        WatchlistEventType::Removed => {
            let _ = c.execute(
                "delete from watchlist_notes where ts_code = ?1",
                params![ts_code],
            );
        }
    }
    Ok(event_id)
}

pub fn note_for(app: &AppHandle, ts_code: &str) -> Result<Option<String>, String> {
    let c = open_database(app)?;
    migrate(&c)?;
    Ok(c.query_row(
        "select note from watchlist_notes where ts_code = ?1",
        params![ts_code],
        |r| r.get::<_, String>(0),
    )
    .ok())
}
