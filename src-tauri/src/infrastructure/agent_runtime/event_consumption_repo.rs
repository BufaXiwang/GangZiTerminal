//! agent_event_consumption —— 跨模块事件 idempotency 记录。

use crate::infrastructure::db::helpers::now;
use crate::infrastructure::db::{migrate, open_database};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tauri::AppHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsumptionStatus {
    Processing,
    Consumed,
    Ignored,
    Failed,
}

impl ConsumptionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ConsumptionStatus::Processing => "processing",
            ConsumptionStatus::Consumed => "consumed",
            ConsumptionStatus::Ignored => "ignored",
            ConsumptionStatus::Failed => "failed",
        }
    }
}

fn conn(app: &AppHandle) -> Result<Connection, String> {
    let c = open_database(app)?;
    migrate(&c)?;
    Ok(c)
}

/// 写入或更新事件消费记录。
///
/// 返回 `true` 表示这是首次见到该 (event_type, event_key, consumer)；
/// 返回 `false` 表示已存在（消费者应按当前 status 判断是否跳过）。
pub fn mark(
    app: &AppHandle,
    event_type: &str,
    event_key: &str,
    consumer: &str,
    status: ConsumptionStatus,
    run_id: Option<&str>,
    error: Option<&str>,
) -> Result<bool, String> {
    let c = conn(app)?;
    let existed: bool = c
        .query_row(
            "select 1 from agent_event_consumption
             where event_type = ?1 and event_key = ?2 and consumer = ?3",
            params![event_type, event_key, consumer],
            |_| Ok(true),
        )
        .unwrap_or(false);
    // spec §8：`consumed` / `ignored` 是终态，不能被覆盖
    c.execute(
        "insert into agent_event_consumption(
            event_type, event_key, consumer, status, run_id, error,
            created_at, updated_at
         ) values (?1,?2,?3,?4,?5,?6, ?7, ?7)
         on conflict(event_type, event_key, consumer) do update set
             status = excluded.status,
             run_id = coalesce(excluded.run_id, agent_event_consumption.run_id),
             error = coalesce(excluded.error, agent_event_consumption.error),
             updated_at = excluded.updated_at
         where agent_event_consumption.status not in ('consumed', 'ignored')",
        params![event_type, event_key, consumer, status.as_str(), run_id, error, now()],
    )
    .map_err(|e| format!("mark event_consumption 失败：{e}"))?;
    Ok(!existed)
}

pub fn status_of(
    app: &AppHandle,
    event_type: &str,
    event_key: &str,
    consumer: &str,
) -> Result<Option<ConsumptionStatus>, String> {
    let c = conn(app)?;
    let s: Option<String> = c
        .query_row(
            "select status from agent_event_consumption
             where event_type = ?1 and event_key = ?2 and consumer = ?3",
            params![event_type, event_key, consumer],
            |r| r.get(0),
        )
        .ok();
    Ok(s.and_then(|x| match x.as_str() {
        "processing" => Some(ConsumptionStatus::Processing),
        "consumed" => Some(ConsumptionStatus::Consumed),
        "ignored" => Some(ConsumptionStatus::Ignored),
        "failed" => Some(ConsumptionStatus::Failed),
        _ => None,
    }))
}
