//! 系统启动时 seed 一批 hand-written 投资原则（heuristics 形态）。
//!
//! 触发：heuristics 表为空（首次启动 / DB 重建后）。
//! 来源：从 identity.md 现有「核心原则 / 决策框架 / 模拟交易边界」提炼，
//! origin=seed（视作"开机时的硬基线"），永远 active，不参与 hit/miss 累加。
//!
//! 取代 v2 seed_principles.rs（W23 删 principle 代码时同步删旧文件）。

use crate::domain::agent::heuristic::{Heuristic, HeuristicCategory};
use crate::domain::shared::OccurredAt;
use crate::infrastructure::agent::heuristic_repo::{count_by_state, create};
use tauri::AppHandle;

const SEED: &[(&str, HeuristicCategory)] = &[
    // 决策框架类
    (
        "信息不足时观察 > 交易；不为了交易而交易",
        HeuristicCategory::Principle,
    ),
    (
        "偏多/偏空判断必须给后续验证清单（写进 expectation.signals_used）",
        HeuristicCategory::Principle,
    ),
    (
        "传闻 / 二手转述 / 未公告小道消息不构成开仓理由",
        HeuristicCategory::Principle,
    ),
    (
        "触及 expectation 失效条件立即平仓，不论盈亏",
        HeuristicCategory::Principle,
    ),
    (
        "归因必须能反推到 expectation 而不是市场情绪——『市场非理性』不是答案",
        HeuristicCategory::Principle,
    ),
    // 风险偏好类
    (
        "默认保守，单笔仓位不重仓；具体上限按 sizing.rs 算",
        HeuristicCategory::RiskPreference,
    ),
    (
        "解禁日 / 财报日附近降低仓位敏感度",
        HeuristicCategory::RiskPreference,
    ),
    // 已知偏差类（A 股结构性陷阱）
    (
        "涨停板 = 流动性断点，不在涨停价追入（买不到 + 次日存在断崖回吐风险）",
        HeuristicCategory::KnownBias,
    ),
    (
        "主板 ±10% / 创业板 ±20% / 科创板 ±20% / 北交所 ±30% 是硬边界",
        HeuristicCategory::KnownBias,
    ),
    (
        "不使用「必涨」「稳赚」等夸张表达——预期会被价格行为验证或证伪",
        HeuristicCategory::KnownBias,
    ),
];

/// 启动时调一次。若 heuristics 表为空 → seed。幂等，重复调安全。
pub fn seed_if_empty(app: &AppHandle) -> Result<(), String> {
    let counts = count_by_state(app)?;
    let total = counts.seed + counts.user_stated + counts.agent_inferred;
    if total > 0 {
        return Ok(());
    }
    let now = OccurredAt::now();
    let mut inserted = 0;
    for (body, category) in SEED {
        let h = Heuristic::seed((*body).to_string(), *category, now);
        create(app, &h)?;
        inserted += 1;
    }
    tracing::info!(count = inserted, "Seed heuristics 已注入");
    Ok(())
}
