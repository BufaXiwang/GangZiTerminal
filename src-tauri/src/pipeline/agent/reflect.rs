//! Reflection pipeline（v3 expectation-driven 重写）。
//!
//! 见 docs/design/agent-v3-expectation-driven.md § 5.4 + § 8。
//!
//! 触发：scheduler 每交易日 15:30 调一次 / Settings 立即触发按钮调一次。
//!
//! Phase 1 设计：**纯代码路径，0 LLM**——
//! 1. `expectation_review::run` 自动判定所有 pending expectations 的 hit/miss/expired
//!    + 写 lessons（takeaway 暂空，Phase 2 LLM 补）+ heuristics application_count 累加
//! 2. `heuristic_emerge::run` 尝试从最近 lessons emerge 新 heuristics
//! 3. 落一条 reflection episode 到 agent_episodes 表（trigger_kind="reflection"）
//!
//! Phase 2 可叠加：让 LLM 跑一遍生成 lessons.takeaway + 写 outcome 自然语言总结。

use crate::pipeline::agent::{expectation_review, heuristic_emerge, observer};
use crate::pipeline::agent::tools::ToolRegistry;
use std::sync::Arc;
use tauri::AppHandle;

#[derive(Debug, Clone)]
pub struct ReflectionResult {
    pub run_id: String,
    pub outcome_summary: String,
    pub thesis_count: usize, // 保留字段名兼容旧前端；语义改为 expectations_reviewed
}

/// 触发一次收盘复盘——可由 scheduler 15:30 tick / Settings 立即按钮调。
///
/// 兼容性：v2 调用方传 `registry` 是 LLM 工具入口；v3 该参数当前不使用（保留以待 Phase 2 升级）。
pub async fn run_close_reflection(
    app: AppHandle,
    _registry: Arc<ToolRegistry>,
) -> Result<ReflectionResult, String> {
    let run_id = uuid::Uuid::new_v4().to_string();
    let started_at = chrono::Utc::now().to_rfc3339();
    // 落 episode 起点（provider/model 留空字符串——本 run 不调 LLM）
    let _ = crate::infrastructure::agent::repository::insert_agent_episode_start(
        &app,
        &run_id,
        "reflection",
        Some("close-15:30"),
        "none", // 不调 provider
        "none", // 不调模型
        &started_at,
        None,
        None,
    );

    // 1. 自动 review pending expectations
    let review = expectation_review::run(&app)?;

    // 2. 尝试 emerge 新 heuristic
    let emerge = heuristic_emerge::run(&app)?;

    let outcome = format!(
        "Review: examined={}, hit={}, missed={}, expired={}, lessons_written={}, heuristic_applications={}. \
         Emerge: clusters={}, new_heuristics={}, duplicates_skipped={}.",
        review.examined,
        review.hit,
        review.missed,
        review.expired,
        review.lessons_written,
        review.heuristic_applications_recorded,
        emerge.clusters_found,
        emerge.heuristics_created,
        emerge.skipped_duplicates,
    );

    // finalize episode（无 LLM 数据，用 0 / "stop" 占位）
    let ended_at = chrono::Utc::now().to_rfc3339();
    let _ = crate::infrastructure::agent::repository::finalize_agent_episode(
        &app,
        &run_id,
        &ended_at,
        0, // turns
        0, 0, 0, 0, 0, 0,
        Some("auto_review_complete"),
        None,
        None,
        Some(&outcome),
    );

    tracing::info!(
        run_id = %run_id,
        outcome = %outcome,
        "Close reflection 完成（纯代码路径）"
    );

    Ok(ReflectionResult {
        run_id,
        outcome_summary: outcome,
        thesis_count: review.examined,
    })
}

// 标记符 observer 模块依赖移除——不再用 observer::start_episode（v2 LLM-driven 路径）
fn _observer_marker() {
    let _ = observer::AGENT_EVENT;
}
