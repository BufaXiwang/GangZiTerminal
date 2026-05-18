//! 系统启动时 seed 一批 hand-written 投资原则。
//!
//! 触发条件：principles 表为空（首次启动 / DB 重建后）。
//! 来源：从 identity.md 现有「核心原则 / 决策框架 / 模拟交易边界」提炼，
//! origin=user_stated（视作"用户写给 agent 的硬规则"），state=active。
//!
//! 见 agent-redesign.md § 5.6。

use crate::domain::agent::principle::{Principle, PrincipleCategory};
use crate::domain::shared::OccurredAt;
use crate::infrastructure::agent::principle_repo::{count_by_state_and_origin, create_principle};
use tauri::AppHandle;

const SEED: &[(&str, PrincipleCategory)] = &[
    // 决策框架类
    (
        "信息不足时观察 > 交易；不为了交易而交易",
        PrincipleCategory::Principle,
    ),
    (
        "偏多/偏空判断必须给后续验证清单（写进 thesis.validation_checks）",
        PrincipleCategory::Principle,
    ),
    (
        "传闻 / 二手转述 / 未公告小道消息不构成开仓理由",
        PrincipleCategory::Principle,
    ),
    (
        "触及 thesis.invalidation 任一条件立即平仓，不论盈亏",
        PrincipleCategory::Principle,
    ),
    // 风险偏好类
    (
        "默认保守，单笔仓位不重仓；具体上限按 sizing.rs 算",
        PrincipleCategory::RiskPreference,
    ),
    (
        "解禁日 / 财报日附近降低仓位敏感度",
        PrincipleCategory::RiskPreference,
    ),
    // 已知偏差类（A 股结构性陷阱）
    (
        "涨停板 = 流动性断点，不在涨停价追入（买不到 + 次日存在断崖回吐风险）",
        PrincipleCategory::KnownBias,
    ),
    (
        "主板 ±10% / 创业板 ±20% / 科创板 ±20% / 北交所 ±30% 是硬边界",
        PrincipleCategory::KnownBias,
    ),
    (
        "不使用「必涨」「稳赚」等夸张表达——模拟盘的判断会被价格行为验证或证伪",
        PrincipleCategory::KnownBias,
    ),
    (
        "归因必须能反推到 thesis 而不是市场情绪——『市场非理性』不是答案",
        PrincipleCategory::Principle,
    ),
];

/// 在启动时检查 principles 表，若为空 → seed。幂等，重复调安全。
pub fn seed_if_empty(app: &AppHandle) -> Result<(), String> {
    let counts = count_by_state_and_origin(app)?;
    let total =
        counts.proposed + counts.active + counts.dormant + counts.retired;
    if total > 0 {
        return Ok(());
    }
    let now = OccurredAt::now();
    let mut inserted = 0;
    for (body, category) in SEED {
        let p = Principle::seed((*body).to_string(), *category, Vec::new(), now);
        create_principle(app, &p)?;
        inserted += 1;
    }
    tracing::info!(count = inserted, "Seed principles 已注入");
    Ok(())
}
