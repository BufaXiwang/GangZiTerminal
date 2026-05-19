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
use crate::domain::account::events::EventSource;
use crate::domain::account::position::CloseReason;
use crate::domain::agent::lesson::{Lesson, LessonOutcome};
use crate::domain::shared::OccurredAt;
use crate::infrastructure::account::{expectation_repo, repository::PositionRepo};
use crate::infrastructure::agent::{
    expectation_heuristic_link_repo, heuristic_repo, lesson_repo, signal_detection_repo,
};
use crate::infrastructure::quotes::snapshot::market_snapshot;
use crate::pipeline::account::AccountService;
use tauri::AppHandle;

#[derive(Debug, Clone)]
pub struct ReviewResult {
    pub examined: usize,
    pub hit: usize,
    pub partial_hit: usize,
    pub missed: usize,
    pub expired: usize,
    /// 因 invalidation_signal 命中提前判 Missed 的条数（计入 missed 总数）
    pub invalidated_by_signal: usize,
    pub lessons_written: usize,
    pub heuristic_applications_recorded: usize,
    pub positions_auto_closed: usize,
}

/// 跑一次 review——扫所有 pending expectations，自动推进状态机。
///
/// `reflection_episode_id`：当 expectation 判定为 Missed 触发自动平仓时，事件源标成
/// `EventSource::Reflection`，便于审计；None 则用 System。
pub async fn run(
    app: &AppHandle,
    reflection_episode_id: Option<String>,
) -> Result<ReviewResult, String> {
    let pending = expectation_repo::list_pending(app, 500)?;
    let mut result = ReviewResult {
        examined: pending.len(),
        hit: 0,
        partial_hit: 0,
        missed: 0,
        expired: 0,
        invalidated_by_signal: 0,
        lessons_written: 0,
        heuristic_applications_recorded: 0,
        positions_auto_closed: 0,
    };
    let account_service = AccountService::new(app.clone());
    let position_repo = PositionRepo::new(app.clone());

    for exp in pending {
        // snapshot key 是 ts_code（带 SH/SZ/BJ 后缀避歧义），不是 6 位裸 code
        let ts_code = exp.code.to_ts_code();
        let Some(quote) = market_snapshot::get(&ts_code) else {
            // 行情没拿到——这条留到下次 review 再判
            tracing::debug!(code = %exp.code, ts_code, "review: 跳过未拿到 quote 的 expectation");
            continue;
        };
        let Some(price) = quote.price else {
            continue;
        };
        let now = OccurredAt::now();

        // 优先级 1：invalidation_signals 提前止损（早于价格判定）
        if let Some(triggered_family) = check_invalidation(app, &exp)? {
            let reason = format!(
                "invalidation_signal 命中：family={triggered_family}（创建后 {} 起检测）",
                exp.created_at.to_rfc3339()
            );
            let event = ExpectationEvent::Missed {
                actual_price: price,
                reason: reason.clone(),
            };
            expectation_repo::transition(app, &exp.id, ExpectationState::Missed, event, now)?;
            result.missed += 1;
            result.invalidated_by_signal += 1;
            if write_lesson(app, &exp, LessonOutcome::Miss, &reason, now).is_ok() {
                result.lessons_written += 1;
            }
            result.heuristic_applications_recorded +=
                record_signal_outcomes(app, &exp, false, now)?;
            let closed = auto_close_linked_positions(
                &account_service,
                &position_repo,
                &exp.id,
                reflection_episode_id.as_deref(),
                CloseReason::Invalidated,
            )
            .await;
            result.positions_auto_closed += closed;
            continue;
        }

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
            OutcomeJudgment::PartialHit {
                actual_price,
                actual_gain_pct,
                target_gain_pct,
                reason,
            } => {
                let event = ExpectationEvent::PartialHit {
                    actual_price,
                    actual_gain_pct,
                    target_gain_pct,
                    reason: reason.clone(),
                };
                expectation_repo::transition(
                    app,
                    &exp.id,
                    ExpectationState::PartialHit,
                    event,
                    now,
                )?;
                result.partial_hit += 1;
                if write_lesson(app, &exp, LessonOutcome::PartialHit, &reason, now).is_ok() {
                    result.lessons_written += 1;
                }
                // PartialHit 不影响 heuristic hit/miss（中性证据），
                // 但仍触发关联 position 自动平仓——按 TimeStop 语义（方向对但节奏没跟上）。
                let closed = auto_close_linked_positions(
                    &account_service,
                    &position_repo,
                    &exp.id,
                    reflection_episode_id.as_deref(),
                    CloseReason::TimeStop,
                )
                .await;
                result.positions_auto_closed += closed;
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
                let closed = auto_close_linked_positions(
                    &account_service,
                    &position_repo,
                    &exp.id,
                    reflection_episode_id.as_deref(),
                    CloseReason::Invalidated,
                )
                .await;
                result.positions_auto_closed += closed;
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

/// 检查 expectation 创建后是否有任一 invalidation_signal family 在 signal_detections 命中。
/// 返回触发的 family（命中即返回 Some，不需要遍历完所有）。
fn check_invalidation(
    app: &AppHandle,
    exp: &Expectation,
) -> Result<Option<String>, String> {
    if exp.invalidation_signals.is_empty() {
        return Ok(None);
    }
    let want: std::collections::HashSet<&str> = exp
        .invalidation_signals
        .iter()
        .map(|s| s.family_str())
        .collect();
    let detections = signal_detection_repo::list_for_code_since(
        app,
        exp.code.as_str(),
        exp.created_at,
    )?;
    for (sig, _ts) in detections {
        if want.contains(sig.family_str()) {
            return Ok(Some(sig.family_str().to_string()));
        }
    }
    Ok(None)
}

/// 把 expectation 终态反向打到关联 Heuristics 的 application_count + hit/miss_count。
///
/// v6 主路径：用 `expectation_heuristic_links` 表精确归因（agent 在 create_expectation 时
/// 显式声明 applied_heuristic_ids）——取代 Phase 1 的"所有 agent_inferred 都给计数"。
///
/// **回落策略**：当 expectation **没有** link 记录（LLM 早期还没学会填 applied_heuristic_ids）
/// 时，按 `expectation.signals_used` 的 family 集合，与 heuristic 的 supporting_lesson_ids
/// 关联 lessons 的 `signals_in_play` family 集合做交集——有交集才计数。比旧的"全部累加"
/// 精确（不会无差别误伤所有 agent_inferred heuristic），又能让早期 heuristic 不至于永远
/// 没证据卡在 probationary。
fn record_signal_outcomes(
    app: &AppHandle,
    exp: &Expectation,
    outcome_hit: bool,
    now: OccurredAt,
) -> Result<usize, String> {
    let linked = expectation_heuristic_link_repo::list_for_expectation(app, &exp.id)?;
    if !linked.is_empty() {
        // 主路径：精确归因
        let mut counted = 0;
        for hid in &linked {
            match heuristic_repo::record_application_outcome(app, hid, outcome_hit, now) {
                Ok(true) => counted += 1,
                Ok(false) => {} // seed/user_stated 拒绝自动注水
                Err(e) => tracing::warn!(
                    expectation = %exp.id,
                    heuristic = %hid,
                    error = %e,
                    "record_application_outcome 失败"
                ),
            }
        }
        return Ok(counted);
    }
    // 回落：按 signal_family 交集匹配
    record_signal_outcomes_by_family_intersect(app, exp, outcome_hit, now)
}

/// 回落归因：找所有 origin=agent_inferred + 未 retired + 至少一条 supporting lesson 的
/// signals_in_play 与本 expectation.signals_used 有 family 交集的 heuristic 给计数。
fn record_signal_outcomes_by_family_intersect(
    app: &AppHandle,
    exp: &Expectation,
    outcome_hit: bool,
    now: OccurredAt,
) -> Result<usize, String> {
    use std::collections::HashSet;

    let exp_families: HashSet<&str> = exp.signals_used.iter().map(|s| s.family_str()).collect();
    if exp_families.is_empty() {
        // expectation 自己没 signals 就别瞎归因
        return Ok(0);
    }
    let all = heuristic_repo::list_all(app, 200)?;
    let mut counted = 0;
    for h in all {
        if h.origin != crate::domain::agent::heuristic::HeuristicOrigin::AgentInferred {
            continue;
        }
        if h.retired_at.is_some() || h.supporting_lesson_ids.is_empty() {
            continue;
        }
        // 检查 supporting lessons 的 signals_in_play family 集是否与本 exp 有交集
        let mut hit_family = false;
        for lid in &h.supporting_lesson_ids {
            let Ok(Some(lesson)) = lesson_repo::get(app, lid) else {
                continue;
            };
            if lesson
                .signals_in_play
                .iter()
                .any(|s| exp_families.contains(s.family_str()))
            {
                hit_family = true;
                break;
            }
        }
        if !hit_family {
            continue;
        }
        match heuristic_repo::record_application_outcome(app, &h.id, outcome_hit, now) {
            Ok(true) => counted += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(
                heuristic = %h.id,
                error = %e,
                "fallback record_application_outcome 失败"
            ),
        }
    }
    Ok(counted)
}

/// 把所有 `current_expectation_id == exp_id` 的活仓位强平掉——
/// expectation 被判定 Missed/PartialHit 时调。Missed 用 Invalidated，PartialHit 用 TimeStop。
///
/// 事件源标 Reflection（如果有 episode_id），否则 System。
/// 异常单条 close 失败不阻断其它仓位 close；返回成功 close 的条数。
async fn auto_close_linked_positions(
    service: &AccountService,
    repo: &PositionRepo,
    exp_id: &ExpectationId,
    reflection_episode_id: Option<&str>,
    close_reason: CloseReason,
) -> usize {
    let Ok(positions) = repo.list_all() else {
        return 0;
    };
    let mut closed = 0;
    for p in positions {
        if !matches!(
            p.status,
            crate::domain::account::position::PositionStatus::Open
        ) {
            continue;
        }
        if p.expectation_id.as_ref().map(|e: &ExpectationId| e.as_str()) != Some(exp_id.as_str()) {
            continue;
        }
        let source = match reflection_episode_id {
            Some(eid) => EventSource::Reflection {
                episode_id: eid.to_string(),
            },
            None => EventSource::System,
        };
        let note = format!(
            "auto-close: expectation {} 判定 {}，按 v3 spec § 5.4 自动平仓",
            exp_id.as_str(),
            close_reason.as_str(),
        );
        match service
            .close_position(&p.id, close_reason, source, note)
            .await
        {
            Ok(_) => {
                closed += 1;
                tracing::info!(
                    expectation = %exp_id,
                    position = %p.id.as_str(),
                    reason = %close_reason.as_str(),
                    "expectation terminal → auto close position"
                );
            }
            Err(err) => {
                tracing::warn!(
                    expectation = %exp_id,
                    position = %p.id.as_str(),
                    error = %err,
                    "auto close position 失败——可能在非交易时段，等下一轮 review 重试"
                );
            }
        }
    }
    closed
}
