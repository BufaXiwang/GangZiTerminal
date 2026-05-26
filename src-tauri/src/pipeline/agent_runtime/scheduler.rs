//! Agent Runtime 后台 tick —— spec `agent-runtime-module.md §5 / §8`。
//!
//! 第一阶段实装：
//! - **news buffer overflow watchdog**：定期检查 `agent_news_buffer`，
//!   当 `pending count >= news_agent_batch_size` 或最老 pending 等待 >=
//!   `news_agent_max_wait_secs` 时，启动 `news_analysis` run（当前阶段
//!   只创建 `AgentRun` 行 + emit `agent-run-started`；真实 LLM loop
//!   后续 phase 接入）。
//!
//! 完整的 scheduled_review tick、quote refresh hook、watchdog 回收
//! `processing` 超时的 event consumption 等 未来落地。

use std::time::Duration;
use tauri::{AppHandle, Emitter};

use crate::domain::agent_runtime::runs::{
    AgentRun, AgentRunProfileId, AgentRunStatus, AgentRunTrigger,
};
use crate::infrastructure::agent_runtime::{locks_ext, news_buffer_repo, runs_repo, settings};
use crate::infrastructure::db::helpers::now;
use crate::pipeline::agent_runtime::locks::InflightGuard;
use crate::pipeline::agent_runtime::router::EVT_AGENT_RUN_STARTED;

const TICK_INTERVAL_SECS: u64 = 30;

pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(news_buffer_loop(app));
}

async fn news_buffer_loop(app: AppHandle) {
    tokio::time::sleep(Duration::from_secs(15)).await;
    loop {
        match tick_news_buffer(&app).await {
            Ok(()) => crate::infrastructure::scheduler_heartbeat::record_ok(
                &app,
                crate::infrastructure::scheduler_heartbeat::LOOP_AGENT_NEWS_BATCH,
            ),
            Err(e) => {
                tracing::warn!(
                    target = "agent_runtime.scheduler",
                    error = %e,
                    "news buffer tick 失败"
                );
                crate::infrastructure::scheduler_heartbeat::record_err(
                    &app,
                    crate::infrastructure::scheduler_heartbeat::LOOP_AGENT_NEWS_BATCH,
                    &e,
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(TICK_INTERVAL_SECS)).await;
    }
}

async fn tick_news_buffer(app: &AppHandle) -> Result<(), String> {
    let Some(guard) = InflightGuard::acquire(locks_ext::LOCK_NEWS_BATCH) else {
        return Ok(());
    };
    let pending = news_buffer_repo::count_pending(app)?;
    if pending == 0 {
        return Ok(());
    }
    let cfg = settings::load(app);
    let age = news_buffer_repo::oldest_pending_age_secs(app)?.unwrap_or(0);
    let should_trigger =
        pending >= cfg.news_agent_batch_size || age >= cfg.news_agent_max_wait_secs;
    if !should_trigger {
        return Ok(());
    }
    let run_id = uuid::Uuid::new_v4().to_string();
    let news_ids = news_buffer_repo::checkout_batch(app, &run_id, cfg.news_agent_batch_size)?;
    // 把 InflightGuard 移交给 run_background_loop 的 on_success 闭包；闭包在
    // run 完成时 drop guard，保证 spec §8「news_batch lock 覆盖整个 run」。
    let mut inflight_guard = Some(guard);
    if news_ids.is_empty() {
        return Ok(());
    }
    // 把本批 news_ids 写入 run_scope，evidence hydrate 时校验 source=packet/news
    {
        use crate::pipeline::agent_runtime::run_scope;
        let mut scope = run_scope::RunScope::default();
        scope.trigger_kind = "news_batch".into();
        for id in &news_ids {
            scope.packet_news_ids.insert(id.clone());
        }
        run_scope::put(&run_id, scope);
    }
    let trigger = AgentRunTrigger::NewsBatch {
        news_ids: news_ids.clone(),
    };
    let run = AgentRun {
        run_id: run_id.clone(),
        profile_id: AgentRunProfileId::NewsAnalysis,
        episode_ids: Vec::new(),
        trigger,
        provider: "deferred".into(),
        wire_format: "messages".into(),
        model: "deferred".into(),
        status: AgentRunStatus::Queued,
        started_at: Some(now()),
        ended_at: None,
        error: None,
    };
    runs_repo::insert(app, &run)?;
    let _ = app.emit(
        EVT_AGENT_RUN_STARTED,
        serde_json::json!({
            "runId": run_id,
            "profileId": "news_analysis",
            "triggerKind": "news_batch",
            "newsCount": news_ids.len(),
        }),
    );
    tracing::info!(
        target = "agent_runtime.scheduler",
        run_id = %run_id,
        pending = pending,
        batched = news_ids.len(),
        age_secs = age,
        "news batch run queued → 启动 LLM loop"
    );

    // spawn 真实 LLM loop。完成后 mark consumed + emit agent-run-finished。
    let app_for_run = app.clone();
    let news_ids_for_run = news_ids.clone();
    let prompt = format!(
        "本批资讯 {} 条：{}。\n\n请按 Agent 纪律：1) 用 fetch_news 拉关注 IDs 的全文摘要；\
         2) 判断是否影响持仓 / 自选 / 触发交易意图；3) 形成判断必须先调 \
         record_decision_episode 记录（即使是 no_action）；4) 需要交易动作再走 \
         operate_account 关联同一 episodeId。",
        news_ids.len(),
        news_ids.join(", ")
    );
    let guard_for_run = inflight_guard.take();
    // spec §8：run 完成 → mark_consumed；run 失败 → mark_failed (retryable retry + terminal)。
    // 把 batch ids 分别 clone 给 success / fail 闭包（FnOnce 不能共享）。
    let news_ids_success = news_ids_for_run.clone();
    let news_ids_fail = news_ids_for_run.clone();
    let max_retries = 3i64;
    let retry_delay_secs = 60i64;
    tauri::async_runtime::spawn(super::background_run::run_background_loop_with_fail(
        app_for_run,
        run_id,
        AgentRunProfileId::NewsAnalysis,
        "news_batch",
        prompt,
        move |inner_app| {
            // 持锁到 run 完成（spec §8）；drop 在闭包 scope 末尾。
            let _guard = guard_for_run;
            if let Err(e) = crate::infrastructure::agent_runtime::news_buffer_repo::mark_consumed(
                inner_app,
                &news_ids_success,
            ) {
                tracing::warn!(
                    target = "agent_runtime.scheduler",
                    error = %e,
                    "mark_consumed 失败"
                );
            }
        },
        move |inner_app, err_msg| {
            // spec §8：retryable=true（agent run 失败一般是 transient），按 max_retries
            // 退避；超过则终态 failed。
            match crate::infrastructure::agent_runtime::news_buffer_repo::mark_failed(
                inner_app,
                &news_ids_fail,
                err_msg,
                true,
                max_retries,
                retry_delay_secs,
            ) {
                Ok((requeued, terminal)) => {
                    tracing::warn!(
                        target = "agent_runtime.scheduler",
                        requeued,
                        terminal,
                        error = %err_msg,
                        "news_batch run 失败，news_buffer 已 retry/terminal"
                    );
                }
                Err(e) => tracing::warn!(
                    target = "agent_runtime.scheduler",
                    error = %e,
                    "mark_failed 失败"
                ),
            }
        },
    ));
    Ok(())
}
