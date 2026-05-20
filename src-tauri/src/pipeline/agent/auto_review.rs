//! Auto review pipeline——纯代码自动判定 open positions 应平仓 + 写 Lesson + 反向打标 heuristic。
//!
//! v4：取代 v3 `expectation_review`——Position 合并 Expectation 后，judge 直接出
//! `CloseReason` 并联动 AccountService.close_position，不再走"先推 expectation
//! 状态再找关联 position"的两段桥。
//!
//! 触发：15:30 reflection tick / 用户手动 trigger。可独立运行（无 LLM）。
//!
//! 流程：
//! 1. 拉所有 status=open 的 positions
//! 2. 对每条调 `judge_position(pos, current_price, now, invalidation_hit)` 纯函数
//! 3. ShouldClose → AccountService.close_position(reason) + 写 Lesson + 反向打标 heuristic
//! 4. Lesson outcome 派生：
//!    - TakeProfit → Hit
//!    - StopLoss / Invalidated → Miss
//!    - TimeStop + no target_price → Expired（中性）
//!    - TimeStop + 方向对未达 → PartialHit（中性）
//!    - TimeStop + 反向 → Miss

use crate::domain::account::events::EventSource;
use crate::domain::account::position::{
    is_partial_hit, judge_position, CloseReason, Position, PositionOutcome,
};
use crate::domain::agent::lesson::{Lesson, LessonOutcome};
use crate::domain::shared::OccurredAt;
use crate::infrastructure::account::repository::PositionRepo;
use crate::infrastructure::agent::{
    heuristic_repo, lesson_repo, position_heuristic_link_repo, signal_detection_repo,
};
use crate::infrastructure::quotes::snapshot::market_snapshot;
use crate::pipeline::account::AccountService;
use tauri::AppHandle;

#[derive(Debug, Clone, Default)]
pub struct ReviewResult {
    pub examined: usize,
    pub hit: usize,
    pub partial_hit: usize,
    pub missed: usize,
    pub expired: usize,
    /// 因 invalidation_signal 命中提前判 Invalidated 的条数（计入 missed 总数）
    pub invalidated_by_signal: usize,
    pub lessons_written: usize,
    pub heuristic_applications_recorded: usize,
    pub positions_auto_closed: usize,
}

/// 跑一次 review——扫所有 open positions，自动平仓 + 写学习闭环。
///
/// `reflection_episode_id`：触发自动平仓时，事件源标 `EventSource::Reflection { episode_id }`。
/// None 则用 System。
pub async fn run(
    app: &AppHandle,
    reflection_episode_id: Option<String>,
) -> Result<ReviewResult, String> {
    let position_repo = PositionRepo::new(app.clone());
    let open: Vec<Position> = position_repo
        .list_open()
        .map_err(|e| format!("list_open 失败：{e}"))?;
    let mut result = ReviewResult {
        examined: open.len(),
        ..Default::default()
    };

    let service = AccountService::new(app.clone());
    let now = OccurredAt::now();

    for pos in open {
        let ts_code = pos.code.to_ts_code();
        let Some(quote) = market_snapshot::get(&ts_code) else {
            tracing::debug!(code = %pos.code, ts_code, "auto_review: 跳过未拿到 quote 的 position");
            continue;
        };
        let Some(price) = quote.price else { continue };

        // 优先查 invalidation_signals 命中
        let invalidation_hit = check_invalidation(app, &pos)?;
        let is_invalidation = invalidation_hit.is_some();

        let outcome = judge_position(&pos, price, now, invalidation_hit);
        let (reason, note) = match outcome {
            PositionOutcome::StillOpen => continue,
            PositionOutcome::ShouldClose { reason, note } => (reason, note),
        };

        let source = match reflection_episode_id.as_deref() {
            Some(eid) => EventSource::Reflection {
                episode_id: eid.to_string(),
            },
            None => EventSource::System,
        };
        let close_note = format!("auto_review: {note}");
        match service
            .close_position(&pos.id, reason, source, close_note)
            .await
        {
            Ok(closed) => {
                result.positions_auto_closed += 1;
                if is_invalidation {
                    result.invalidated_by_signal += 1;
                }
                let (lesson_outcome, neutral) = derive_lesson_outcome(&closed, reason, &price);
                match lesson_outcome {
                    LessonOutcome::Hit => result.hit += 1,
                    LessonOutcome::Miss => result.missed += 1,
                    LessonOutcome::PartialHit => result.partial_hit += 1,
                    LessonOutcome::Expired => result.expired += 1,
                }
                if write_lesson(app, &closed, lesson_outcome, &note, now).is_ok() {
                    result.lessons_written += 1;
                }
                if !neutral {
                    let hit = matches!(lesson_outcome, LessonOutcome::Hit);
                    result.heuristic_applications_recorded +=
                        record_signal_outcomes(app, &closed, hit, now)?;
                }
            }
            Err(e) => {
                tracing::warn!(
                    position = %pos.id.as_str(),
                    reason = %reason.as_str(),
                    error = %e,
                    "auto_review close_position 失败——等下一轮 review 重试"
                );
            }
        }
    }

    Ok(result)
}

/// 检查 position 创建后是否有任一 invalidation_signal family 在 signal_detections 命中。
/// 返回命中的 family（命中即返回 Some，不需要遍历完所有）。
fn check_invalidation(app: &AppHandle, pos: &Position) -> Result<Option<String>, String> {
    if pos.invalidation_signals.is_empty() {
        return Ok(None);
    }
    let want: std::collections::HashSet<&str> = pos
        .invalidation_signals
        .iter()
        .map(|s| s.family_str())
        .collect();
    let detections =
        signal_detection_repo::list_for_code_since(app, pos.code.as_str(), pos.entered_at)?;
    for (sig, _ts) in detections {
        if want.contains(sig.family_str()) {
            return Ok(Some(sig.family_str().to_string()));
        }
    }
    Ok(None)
}

/// 根据 CloseReason + position 字段派生 Lesson outcome + 是否中性。
///
/// 中性 = 不计 heuristic hit/miss（partial_hit / expired）
fn derive_lesson_outcome(
    pos: &Position,
    reason: CloseReason,
    exit_price: &crate::domain::shared::Yuan,
) -> (LessonOutcome, bool) {
    match reason {
        CloseReason::TakeProfit => (LessonOutcome::Hit, false),
        CloseReason::StopLoss | CloseReason::Invalidated => (LessonOutcome::Miss, false),
        CloseReason::TimeStop => {
            if pos.take_profit.is_none() {
                (LessonOutcome::Expired, true)
            } else if is_partial_hit(pos.direction, pos.avg_entry_price, *exit_price, pos.take_profit) {
                (LessonOutcome::PartialHit, true)
            } else {
                (LessonOutcome::Miss, false)
            }
        }
        // Manual close 不进 auto_review，但万一上游传过来按 Expired 处理
        CloseReason::Manual => (LessonOutcome::Expired, true),
    }
}

/// 根据 position 终态自动生成一条 Lesson。
/// observation 由代码生成（客观事实），takeaway 留空字符串——reflect.rs LLM 后续填。
fn write_lesson(
    app: &AppHandle,
    pos: &Position,
    outcome: LessonOutcome,
    reason: &str,
    now: OccurredAt,
) -> Result<(), String> {
    let observation = format!(
        "position {} ({}, kind={}, direction={}, take_profit={:?}, stop_loss={:?}): {}",
        pos.id.as_str(),
        pos.code.as_str(),
        pos.kind.as_str(),
        pos.direction.as_str(),
        pos.take_profit.as_ref().map(|y| y.value()),
        pos.stop_loss.as_ref().map(|y| y.value()),
        reason,
    );
    let lesson = Lesson::new(
        pos.id.clone(),
        pos.code.clone(),
        observation,
        String::new(),
        outcome,
        None, // regime_at_close：未接 regime detector 时留空
        pos.signals_used.clone(),
        None, // pnl_pct：可从事件链算，phase 1 留空
        now,
    );
    lesson_repo::create(app, &lesson)?;
    Ok(())
}

/// 把 position 终态反向打到关联 Heuristics 的 application_count + hit/miss_count。
///
/// 主路径：用 `position_heuristic_links` 表精确归因（agent 在 open_position 时
/// 显式声明 applied_heuristic_ids）。
///
/// **回落策略**：当 position **没有** link 记录时，按 `position.signals_used` 的 family
/// 集合，与 heuristic 的 supporting_lesson_ids 关联 lessons 的 `signals_in_play` family
/// 集合做交集——有交集才计数。比"全部累加"精确（不会无差别误伤无关 heuristic），
/// 又能让早期 heuristic 不至于永远没证据卡在 probationary。
fn record_signal_outcomes(
    app: &AppHandle,
    pos: &Position,
    outcome_hit: bool,
    now: OccurredAt,
) -> Result<usize, String> {
    let linked = position_heuristic_link_repo::list_for_position(app, &pos.id)?;
    if !linked.is_empty() {
        let mut counted = 0;
        for hid in &linked {
            match heuristic_repo::record_application_outcome(app, hid, outcome_hit, now) {
                Ok(true) => counted += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    position = %pos.id,
                    heuristic = %hid,
                    error = %e,
                    "record_application_outcome 失败"
                ),
            }
        }
        return Ok(counted);
    }
    record_signal_outcomes_by_family_intersect(app, pos, outcome_hit, now)
}

/// 回落归因：找所有 origin=agent_inferred + 未 retired + 至少一条 supporting lesson 的
/// signals_in_play 与本 position.signals_used 有 family 交集的 heuristic 给计数。
fn record_signal_outcomes_by_family_intersect(
    app: &AppHandle,
    pos: &Position,
    outcome_hit: bool,
    now: OccurredAt,
) -> Result<usize, String> {
    use std::collections::HashSet;

    let exp_families: HashSet<&str> = pos.signals_used.iter().map(|s| s.family_str()).collect();
    if exp_families.is_empty() {
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
