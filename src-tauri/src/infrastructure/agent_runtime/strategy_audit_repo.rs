//! strategy_card_audit —— spec §2 `StrategyCard.version` 历史追溯。
//!
//! 每次 upsert（含首次创建）必须在 audit 表 append 一条快照，便于：
//! - 复盘时跳回 episode 引用的策略版本
//! - UI 展示策略变更时间线
//!
//! 表 schema 在 migrations.rs；本 repo 只暴露 `append` / `list_by_strategy`。

use rusqlite::{params, Connection};
use tauri::AppHandle;

use crate::domain::agent_runtime::decisions::StrategyCard;
use crate::infrastructure::db::helpers::now;
use crate::infrastructure::db::{migrate, open_database};

fn conn(app: &AppHandle) -> Result<Connection, String> {
    let c = open_database(app)?;
    migrate(&c)?;
    Ok(c)
}

pub fn append(app: &AppHandle, card: &StrategyCard, reason: &str) -> Result<(), String> {
    let c = conn(app)?;
    let config_json =
        serde_json::to_string(&card.config).map_err(|e| format!("config 序列化失败：{e}"))?;
    let status_str = match card.status {
        crate::domain::agent_runtime::decisions::StrategyStatus::Active => "active",
        crate::domain::agent_runtime::decisions::StrategyStatus::Paused => "paused",
    };
    c.execute(
        "insert into strategy_card_audit(
            strategy_id, version, name, description, status, config_json, reason, recorded_at
         ) values (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            card.strategy_id,
            card.version,
            card.name,
            card.description,
            status_str,
            config_json,
            reason,
            now(),
        ],
    )
    .map_err(|e| format!("写 strategy_card_audit 失败：{e}"))?;
    Ok(())
}

