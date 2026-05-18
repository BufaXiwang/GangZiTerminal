//! Agent 机制健康度指标——day 1 即可观测，不依赖 baseline。
//!
//! 见 docs/design/agent-redesign.md § 5.5。
//!
//! **不是收益验证**——验证 agent 有效性的 ground truth 是模拟账户 PnL（account 主指标）。
//! 本模块只回答"机制是不是在按设计跑"，防止"系统在跑但跑歪了"。
//!
//! 6 个指标（按 v1 spec）：
//! 1. Thesis 完整度：新建 thesis 中带 invalidation + ≥2 validation_checks 的比例
//! 2. Reflection 触发率（W3 stub）：closed/invalidated thesis 是否有对应 reflection episode
//! 3. Reflection 对照率（W3 stub）：reflection 文本里出现 invalidation/validation 关键词
//! 4. Principle 流动性：proposed→active + active→dormant 流转计数
//! 5. Principle origin 分布：agent_inferred 占比（应随时间上升）
//! 6. Regime 切换检测（W3 stub）：principles.regime_tags 多样性

use crate::infrastructure::agent::principle_repo::count_by_state_and_origin;
use crate::infrastructure::db::{migrate, open_database};
use serde::Serialize;
use tauri::AppHandle;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthMetrics {
    pub thesis_completeness_rate: Option<f64>, // 0.0-1.0；样本不足返 None
    pub total_theses: u32,
    pub total_closed_theses: u32,
    pub reflection_episode_count_7d: u32,
    pub principle_state_counts: PrincipleStateCounts,
    pub principle_origin_share: PrincipleOriginShare,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrincipleStateCounts {
    pub proposed: u32,
    pub active: u32,
    pub dormant: u32,
    pub retired: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrincipleOriginShare {
    pub user_stated: u32,
    pub agent_inferred: u32,
    /// agent_inferred / (user_stated + agent_inferred)；分母为 0 时 None
    pub agent_inferred_share: Option<f64>,
}

pub fn compute(app: &AppHandle) -> Result<HealthMetrics, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;

    // 1. Thesis 完整度
    let (total_theses, complete_count): (u32, u32) = conn
        .query_row(
            "select
                count(*),
                sum(case when length(invalidation) > 0
                          and validation_checks is not null
                          and json_array_length(validation_checks) >= 2
                         then 1 else 0 end)
             from theses",
            [],
            |row| {
                let total: u32 = row.get(0).unwrap_or(0);
                let complete: Option<u32> = row.get(1).ok();
                Ok((total, complete.unwrap_or(0)))
            },
        )
        .unwrap_or((0, 0));
    let thesis_completeness_rate = if total_theses > 0 {
        Some(complete_count as f64 / total_theses as f64)
    } else {
        None
    };

    // closed theses
    let total_closed_theses: u32 = conn
        .query_row(
            "select count(*) from theses where state in ('validated','drifted','invalidated','abandoned')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // 最近 7 天 reflection episode 数量
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(7)).to_rfc3339();
    let reflection_episode_count_7d: u32 = conn
        .query_row(
            "select count(*) from agent_episodes where trigger_kind='reflection' and started_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Principle 计数
    let counts = count_by_state_and_origin(app)?;
    let total_origin = counts.user_stated + counts.agent_inferred;
    let agent_inferred_share = if total_origin > 0 {
        Some(counts.agent_inferred as f64 / total_origin as f64)
    } else {
        None
    };

    Ok(HealthMetrics {
        thesis_completeness_rate,
        total_theses,
        total_closed_theses,
        reflection_episode_count_7d,
        principle_state_counts: PrincipleStateCounts {
            proposed: counts.proposed,
            active: counts.active,
            dormant: counts.dormant,
            retired: counts.retired,
        },
        principle_origin_share: PrincipleOriginShare {
            user_stated: counts.user_stated,
            agent_inferred: counts.agent_inferred,
            agent_inferred_share,
        },
    })
}
