//! strategy_cards 持久化 + baseline seed。

use crate::infrastructure::db::helpers::now;
use crate::infrastructure::db::{migrate, open_database};
use crate::domain::agent_runtime::decisions::{
    StrategyCard, StrategyCardConfig, StrategyStatus,
};
use rusqlite::{params, Connection};
use tauri::AppHandle;

fn conn(app: &AppHandle) -> Result<Connection, String> {
    let c = open_database(app)?;
    migrate(&c)?;
    Ok(c)
}

fn status_str(s: StrategyStatus) -> &'static str {
    match s {
        StrategyStatus::Active => "active",
        StrategyStatus::Paused => "paused",
    }
}

fn parse_status(s: &str) -> StrategyStatus {
    match s {
        "active" => StrategyStatus::Active,
        _ => StrategyStatus::Paused,
    }
}

pub fn upsert(app: &AppHandle, card: &StrategyCard) -> Result<(), String> {
    let c = conn(app)?;
    let cfg =
        serde_json::to_string(&card.config).map_err(|e| format!("config 序列化失败：{e}"))?;
    c.execute(
        "insert into strategy_cards(
            strategy_id, version, name, description, status, config_json,
            created_at, updated_at
         ) values (?1,?2,?3,?4,?5,?6, ?7, ?8)
         on conflict(strategy_id) do update set
             version = excluded.version,
             name = excluded.name,
             description = excluded.description,
             status = excluded.status,
             config_json = excluded.config_json,
             updated_at = excluded.updated_at",
        params![
            card.strategy_id,
            card.version,
            card.name,
            card.description,
            status_str(card.status),
            cfg,
            card.created_at,
            card.updated_at,
        ],
    )
    .map_err(|e| format!("upsert strategy_card 失败：{e}"))?;
    Ok(())
}

pub fn list(app: &AppHandle, status_filter: Option<&str>) -> Result<Vec<StrategyCard>, String> {
    let c = conn(app)?;
    let (sql, want_filter) = if status_filter.is_some() {
        (
            "select strategy_id, version, name, description, status, config_json,
                    created_at, updated_at
             from strategy_cards where status = ?1 order by updated_at desc",
            true,
        )
    } else {
        (
            "select strategy_id, version, name, description, status, config_json,
                    created_at, updated_at
             from strategy_cards order by updated_at desc",
            false,
        )
    };
    let mut stmt = c.prepare(sql).map_err(|e| format!("prepare 失败：{e}"))?;
    let mut rows = if want_filter {
        stmt.query(params![status_filter.unwrap()])
    } else {
        stmt.query([])
    }
    .map_err(|e| format!("query 失败：{e}"))?;

    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(|e| format!("next 失败：{e}"))? {
        let cfg_json: String = r.get(5).map_err(|e| e.to_string())?;
        let config: StrategyCardConfig =
            serde_json::from_str(&cfg_json).unwrap_or_default();
        out.push(StrategyCard {
            strategy_id: r.get(0).map_err(|e| e.to_string())?,
            version: r.get::<_, i64>(1).map_err(|e| e.to_string())? as u32,
            name: r.get(2).map_err(|e| e.to_string())?,
            description: r.get(3).map_err(|e| e.to_string())?,
            status: parse_status(&r.get::<_, String>(4).map_err(|e| e.to_string())?),
            config,
            created_at: r.get(6).map_err(|e| e.to_string())?,
            updated_at: r.get(7).map_err(|e| e.to_string())?,
        });
    }
    Ok(out)
}

pub fn count(app: &AppHandle) -> Result<i64, String> {
    let c = conn(app)?;
    let n: i64 = c
        .query_row("select count(*) from strategy_cards", [], |r| r.get(0))
        .map_err(|e| format!("count 失败：{e}"))?;
    Ok(n)
}

/// spec §2 `StrategyCard`：首次启动若没有 active strategy，必须 seed baseline。
pub fn seed_baseline_if_empty(app: &AppHandle) -> Result<(), String> {
    if count(app)? > 0 {
        return Ok(());
    }
    let card = StrategyCard {
        strategy_id: "baseline_a_share_risk_control".to_string(),
        version: 1,
        name: "A 股基线风控".to_string(),
        description: "baseline 投资纪律：仓位上限 / freshness / 止损 / 不确定不交易"
            .to_string(),
        status: StrategyStatus::Active,
        config: StrategyCardConfig {
            entry_rules: vec![
                "stale 或缺失行情时不下市价单".to_string(),
                "评估盘口可成交性后再开仓".to_string(),
            ],
            exit_rules: vec!["命中 invalidation 信号必须显式平仓或调整保护".to_string()],
            risk_rules: vec![
                "单票仓位不超过 25%".to_string(),
                "总仓位不超过 95%".to_string(),
                "单笔订单金额不超过 25% riskEquity".to_string(),
            ],
            factor_weights: None,
            applicable_regimes: Vec::new(),
        },
        created_at: now(),
        updated_at: now(),
    };
    upsert(app, &card)?;
    super::strategy_audit_repo::append(app, &card, "seed baseline_a_share_risk_control")
}
