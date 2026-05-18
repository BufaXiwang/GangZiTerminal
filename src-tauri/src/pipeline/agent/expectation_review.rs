//! Expectation review pipeline——纯代码自动判定 expectation 终态 + 写 Lesson + 触发学习计数。
//!
//! 见 docs/design/agent-v3-expectation-driven.md § 5.4 + § 8.1。
//!
//! 触发：15:30 reflection tick / 用户手动 trigger。可独立运行（无 LLM）。
//!
//! 流程：
//! 1. 拉所有 state=pending 的 expectations
//! 2. 对每条调 `judge_outcome(exp, current_price, now)` 纯函数
//! 3. 终态（Hit/Missed/Expired）→ 写 expectation_events + transition state + 写 Lesson
//! 4. signals_used 数组里每个 SignalKind 反向标 hit/miss → 关联到 Heuristic application_count
//! 5. expectation 关联的 active position 在 Missed 时自动 close（reason=Invalidated, source=Reflection）
//!
//! Phase 1 简化：takeaway 由调用方（reflect.rs）传入；本模块只生成 observation。

use crate::domain::account::expectation::{
    judge_outcome, Expectation, ExpectationEvent, ExpectationId, ExpectationState, OutcomeJudgment,
};
use crate::domain::agent::lesson::{Lesson, LessonOutcome};
use crate::domain::shared::OccurredAt;
use crate::infrastructure::account::expectation_repo;
use crate::infrastructure::agent::{heuristic_repo, lesson_repo};
use crate::infrastructure::quotes::snapshot::market_snapshot;
use tauri::AppHandle;

#[derive(Debug, Clone)]
pub struct ReviewResult {
    pub examined: usize,
    pub hit: usize,
    pub missed: usize,
    pub expired: usize,
    pub lessons_written: usize,
    pub heuristic_applications_recorded: usize,
}

/// 跑一次 review——扫所有 pending expectations，自动推进状态机。
pub fn run(app: &AppHandle) -> Result<ReviewResult, String> {
    let pending = expectation_repo::list_pending(app, 500)?;
    let mut result = ReviewResult {
        examined: pending.len(),
        hit: 0,
        missed: 0,
        expired: 0,
        lessons_written: 0,
        heuristic_applications_recorded: 0,
    };

    for exp in pending {
        let Some(quote) = market_snapshot::get(exp.code.as_str()) else {
            // 行情没拿到——这条留到下次 review 再判
            tracing::debug!(code = %exp.code, "review: 跳过未拿到 quote 的 expectation");
            continue;
        };
        let Some(price) = quote.price else {
            continue;
        };
        let now = OccurredAt::now();
        let outcome = judge_outcome(&exp, price, now);
        match outcome {
            OutcomeJudgment::StillPending => continue,
            OutcomeJudgment::Hit { actual_price, reason } => {
                let event = ExpectationEvent::Hit {
                    actual_price,
                    reason: reason.clone(),
                };
                expectation_repo::transition(app, &exp.id, ExpectationState::Hit, event, now)?;
                result.hit += 1;
                if write_lesson(app, &exp, LessonOutcome::Hit, &reason, now).is_ok() {
                    result.lessons_written += 1;
                }
                result.heuristic_applications_recorded +=
                    record_signal_outcomes(app, &exp, true, now)?;
            }
            OutcomeJudgment::Missed { actual_price, reason } => {
                let event = ExpectationEvent::Missed {
                    actual_price,
                    reason: reason.clone(),
                };
                expectation_repo::transition(app, &exp.id, ExpectationState::Missed, event, now)?;
                result.missed += 1;
                if write_lesson(app, &exp, LessonOutcome::Miss, &reason, now).is_ok() {
                    result.lessons_written += 1;
                }
                result.heuristic_applications_recorded +=
                    record_signal_outcomes(app, &exp, false, now)?;
                // 自动平仓——v3 spec § 19 FAQ：agent 主动建仓 → 关联 position 在 Missed 时自动平
                // Phase 1 实现：标记 + 日志；真正写 close 路径在 W24 wire 到 AccountService 时补
                tracing::info!(
                    expectation = %exp.id,
                    code = %exp.code,
                    "expectation missed → TODO 自动平仓（W24 接 AccountService 后实现）"
                );
            }
            OutcomeJudgment::Expired { reason } => {
                let event = ExpectationEvent::Expired {
                    reason: reason.clone(),
                };
                expectation_repo::transition(app, &exp.id, ExpectationState::Expired, event, now)?;
                result.expired += 1;
                if write_lesson(app, &exp, LessonOutcome::Expired, &reason, now).is_ok() {
                    result.lessons_written += 1;
                }
                // expired 不计 hit/miss——节奏判断错而已
            }
        }
    }

    Ok(result)
}

/// 根据 expectation 终态自动生成一条 Lesson。
/// observation 由代码生成（客观事实），takeaway 留空字符串——Phase 1 由 reflect.rs LLM 后续填。
fn write_lesson(
    app: &AppHandle,
    exp: &Expectation,
    outcome: LessonOutcome,
    reason: &str,
    now: OccurredAt,
) -> Result<(), String> {
    let observation = format!(
        "expectation {} ({}, direction={}, target={:?}, horizon={}d): {}",
        exp.id.as_str(),
        exp.code.as_str(),
        exp.direction.as_str(),
        exp.target_price.as_ref().map(|y| y.value()),
        exp.horizon_days,
        reason,
    );
    let lesson = Lesson::new(
        exp.id.clone(),
        exp.code.clone(),
        observation,
        String::new(), // takeaway 由 reflect.rs LLM 填
        outcome,
        exp.regime_at_creation,
        exp.signals_used.clone(),
        None, // pnl_pct 在 W24 wire account 后补
        now,
    );
    lesson_repo::create(app, &lesson)?;
    Ok(())
}

/// 把 expectation 终态反向打到关联 Heuristics 的 application_count + hit/miss_count。
///
/// Phase 1 简化策略：用 expectation 触发时记录的 `signals_used`，匹配最近的
/// `origin=agent_inferred` 且 supporting_lesson_ids 共享同一 signal_family 的 heuristic。
/// 若关联不到现成 heuristic（常见——刚开始没 heuristic）→ 静默跳过，等 heuristic_emerge 后续 emerge。
fn record_signal_outcomes(
    app: &AppHandle,
    exp: &Expectation,
    outcome_hit: bool,
    now: OccurredAt,
) -> Result<usize, String> {
    // Phase 1：粗暴聚合——找所有 supporting_lesson_ids 关联任意 exp 同 signal_family 的 heuristic
    // 给它们 +1 application。Phase 2 可以更精细（按 lesson 的 signals_in_play 精确匹配）。
    let all = heuristic_repo::list_all(app, 200)?;
    let mut counted = 0;
    for h in all {
        if h.origin != crate::domain::agent::heuristic::HeuristicOrigin::AgentInferred {
            continue;
        }
        if h.retired_at.is_some() {
            continue;
        }
        // 简化：暂时只对 supporting_lesson_ids 非空的 agent_inferred heuristic 累计
        // （表示这是 emerge 出来的有支持的 heuristic）
        if h.supporting_lesson_ids.is_empty() {
            continue;
        }
        // 这里如果想精细匹配，需要 join lessons.signals_in_play
        // Phase 1 简化：所有 agent_inferred + 有 lessons 支持的都给计数
        let recorded = heuristic_repo::record_application_outcome(app, &h.id, outcome_hit, now)?;
        if recorded {
            counted += 1;
        }
    }
    let _ = exp;
    Ok(counted)
}
