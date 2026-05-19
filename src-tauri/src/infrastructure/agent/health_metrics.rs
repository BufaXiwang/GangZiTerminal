//! Agent 机制健康度指标——v3 expectation-driven 版。
//!
//! 见 docs/design/agent-v3-expectation-driven.md § 9.10。
//!
//! **不是收益验证**——验证 agent 有效性的 ground truth 是模拟账户 PnL（主指标）。
//! 本模块只回答"机制是不是在按设计跑"。
//!
//! 6 个指标：
//! 1. Expectation 完整度：signals_used ≥1 且 target_price 非空的比例
//! 2. 7 天 reflection 触发率
//! 3. 7 天 scan tick 触发数
//! 4. Heuristic state 分布（按 origin: seed / user_stated / agent_inferred）
//! 5. Heuristic origin 流动性：agent_inferred 占比（agent 真在学的信号）
//! 6. Lesson 7 天累积数

use crate::infrastructure::agent::heuristic_repo::count_by_state;
use crate::infrastructure::db::{migrate, open_database};
use crate::infrastructure::scheduler_heartbeat::{list_heartbeats, HeartbeatRow};
use serde::Serialize;
use tauri::AppHandle;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthMetrics {
    pub expectation_completeness_rate: Option<f64>,
    pub total_expectations: u32,
    pub total_closed_expectations: u32,
    pub reflection_episode_count_7d: u32,
    pub scan_tick_count_7d: u32,
    pub lessons_count_7d: u32,
    pub heuristic_counts: HeuristicCountsDto,
    pub heuristic_origin_share: HeuristicOriginShare,

    // v5 自迭代审计字段——直接对应 docs/architecture.md 里 "5 秒确认查询" 的几条
    /// 今天（北京时间 0 点起）的 scan tick 数；0 说明 scan_scheduler 没在跑
    pub scan_ticks_today: u32,
    /// 今天新生成的 expectation 数；连续多日为 0 → agent 没在产出预期
    pub expectations_created_today: u32,
    /// 今天 reflection 写入的 lesson 数
    pub lessons_created_today: u32,
    /// 最近 7 天 takeaway 仍为空的 lesson 数——> 0 说明 LLM provider 没接通或 takeaway fill 在失败
    pub lessons_empty_takeaway_7d: u32,
    /// 7 天内新 emerge 的 heuristic 数；连续 2 周为 0 → emerge 链路死了
    pub heuristics_emerged_7d: u32,
    /// 各 loop 心跳——直接展示给用户看哪条卡了
    pub heartbeats: Vec<HeartbeatRow>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeuristicCountsDto {
    pub seed: u32,
    pub user_stated: u32,
    pub agent_inferred: u32,
    pub retired: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeuristicOriginShare {
    pub seed: u32,
    pub user_stated: u32,
    pub agent_inferred: u32,
    /// agent_inferred / (seed + user_stated + agent_inferred)——agent 真在学的信号
    pub agent_inferred_share: Option<f64>,
}

pub fn compute(app: &AppHandle) -> Result<HealthMetrics, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;

    // 1. Expectation 完整度
    let (total_expectations, complete_count): (u32, u32) = conn
        .query_row(
            "select
                count(*),
                sum(case when length(reasoning) > 0
                          and signals_used is not null
                          and json_array_length(signals_used) >= 1
                          and target_price is not null
                         then 1 else 0 end)
             from expectations",
            [],
            |row| {
                let total: u32 = row.get(0).unwrap_or(0);
                let complete: Option<u32> = row.get(1).ok();
                Ok((total, complete.unwrap_or(0)))
            },
        )
        .unwrap_or((0, 0));
    let expectation_completeness_rate = if total_expectations > 0 {
        Some(complete_count as f64 / total_expectations as f64)
    } else {
        None
    };

    let total_closed_expectations: u32 = conn
        .query_row(
            "select count(*) from expectations where state in ('hit','missed','expired','cancelled','superseded')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let cutoff_7d = (chrono::Utc::now() - chrono::Duration::days(7)).to_rfc3339();
    let reflection_episode_count_7d: u32 = conn
        .query_row(
            "select count(*) from agent_episodes where trigger_kind='reflection' and started_at >= ?1",
            rusqlite::params![cutoff_7d],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let scan_tick_count_7d: u32 = conn
        .query_row(
            "select count(*) from agent_episodes where trigger_kind='scan' and started_at >= ?1",
            rusqlite::params![cutoff_7d],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let lessons_count_7d: u32 = conn
        .query_row(
            "select count(*) from lessons where created_at >= ?1",
            rusqlite::params![cutoff_7d],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let counts = count_by_state(app)?;
    let total_origin = counts.seed + counts.user_stated + counts.agent_inferred;
    let agent_inferred_share = if total_origin > 0 {
        Some(counts.agent_inferred as f64 / total_origin as f64)
    } else {
        None
    };

    // 北京时间 0 点的 RFC3339 等价（UTC = 北京时间 - 8h）—— start_of_today_utc
    let beijing_now = chrono::Utc::now() + chrono::Duration::hours(8);
    let today_start_beijing = beijing_now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight always valid");
    // 转回 UTC（北京 0 点 = 前一天 UTC 16:00），SQLite 表里时间戳都是 UTC RFC3339
    let today_start_utc = today_start_beijing - chrono::Duration::hours(8);
    let today_cutoff = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
        today_start_utc,
        chrono::Utc,
    )
    .to_rfc3339();

    let scan_ticks_today: u32 = conn
        .query_row(
            "select count(*) from agent_episodes where trigger_kind='scan' and started_at >= ?1",
            rusqlite::params![today_cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let expectations_created_today: u32 = conn
        .query_row(
            "select count(*) from expectations where created_at >= ?1",
            rusqlite::params![today_cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let lessons_created_today: u32 = conn
        .query_row(
            "select count(*) from lessons where created_at >= ?1",
            rusqlite::params![today_cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let lessons_empty_takeaway_7d: u32 = conn
        .query_row(
            "select count(*) from lessons where takeaway='' and created_at >= ?1",
            rusqlite::params![cutoff_7d],
            |row| row.get(0),
        )
        .unwrap_or(0);
    // 7 天内 emerge——优先看 last_emerged_at，没有就回落 created_at + origin=agent_inferred
    let heuristics_emerged_7d: u32 = conn
        .query_row(
            "select count(*) from heuristics
             where (last_emerged_at is not null and last_emerged_at >= ?1)
                or (last_emerged_at is null and origin='agent_inferred' and created_at >= ?1)",
            rusqlite::params![cutoff_7d],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let heartbeats = list_heartbeats(app).unwrap_or_default();

    Ok(HealthMetrics {
        expectation_completeness_rate,
        total_expectations,
        total_closed_expectations,
        reflection_episode_count_7d,
        scan_tick_count_7d,
        lessons_count_7d,
        heuristic_counts: HeuristicCountsDto {
            seed: counts.seed,
            user_stated: counts.user_stated,
            agent_inferred: counts.agent_inferred,
            retired: counts.retired,
        },
        heuristic_origin_share: HeuristicOriginShare {
            seed: counts.seed,
            user_stated: counts.user_stated,
            agent_inferred: counts.agent_inferred,
            agent_inferred_share,
        },
        scan_ticks_today,
        expectations_created_today,
        lessons_created_today,
        lessons_empty_takeaway_7d,
        heuristics_emerged_7d,
        heartbeats,
    })
}
