//! 跨模块事件订阅 → AgentRun 调度路由。
//!
//! 对齐 docs/design/agent-runtime-module.md §5 + §6。
//!
//! 实装：
//! - `news-refreshed` → 把 newIds / updatedIds / articleUpdatedNewsIds 写入
//!   `agent_news_buffer`，并写 event_consumption 幂等记录。Agent run 触发
//!   走 buffer overflow watchdog（scheduler::news_buffer_loop）+
//!   run_background_loop。
//! - `account-triggered` → 以 trigger_id 做 event_key 写 event_consumption +
//!   inflight lock + spawn run_background_loop；run 完成后 mark consumed +
//!   `Account.mark_trigger_handled`。
//! - `market-quotes-refreshed` → AccountService.snapshot 重建 + evaluate triggers
//!   由 pipeline/scheduler::account_snapshot_loop 处理；本模块不重复订阅。

use tauri::{AppHandle, Emitter, Listener};

use crate::domain::agent_runtime::runs::{
    AgentRun, AgentRunProfileId, AgentRunStatus, AgentRunTrigger,
};
use crate::domain::shared::{AccountTriggeredPayload, NewsRefreshedPayload};
use crate::infrastructure::agent_runtime::{
    event_consumption_repo, news_buffer_repo, runs_repo, trade_intents_repo,
};
use crate::infrastructure::db::helpers::now;
use event_consumption_repo::ConsumptionStatus;

pub const EVT_NEWS_REFRESHED: &str = "news-refreshed";
pub const EVT_MARKET_QUOTES_REFRESHED: &str = "market-quotes-refreshed";
pub const EVT_ACCOUNT_TRIGGERED: &str = "account-triggered";
#[allow(dead_code)] // spec §6 应用事件名空间，UI 侧会监听
pub const EVT_ACCOUNT_UPDATED: &str = "account-updated";
pub const EVT_AGENT_RUN_STARTED: &str = "agent-run-started";
pub const EVT_AGENT_RUN_FINISHED: &str = "agent-run-finished";

const CONSUMER: &str = "agent_runtime";

pub fn install_listeners(app: AppHandle) {
    install_news_listener(app.clone());
    install_market_quotes_listener(app.clone());
    install_account_triggered_listener(app);
}

fn install_news_listener(app: AppHandle) {
    let app_for_handler = app.clone();
    app.listen(EVT_NEWS_REFRESHED, move |event| {
        let app = app_for_handler.clone();
        let payload = event.payload().to_string();
        tauri::async_runtime::spawn(async move {
            let parsed: NewsRefreshedPayload = match serde_json::from_str(&payload) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        target = "agent_runtime.router",
                        error = %e,
                        "news-refreshed payload 解析失败"
                    );
                    return;
                }
            };
            if parsed.batch_id.is_empty() {
                return;
            }
            // 已消费过的 batch_id 不重复处理
            match event_consumption_repo::status_of(
                &app,
                EVT_NEWS_REFRESHED,
                &parsed.batch_id,
                CONSUMER,
            ) {
                Ok(Some(ConsumptionStatus::Consumed | ConsumptionStatus::Ignored)) => return,
                _ => {}
            }
            let mut changed: Vec<String> = parsed
                .new_ids
                .into_iter()
                .chain(parsed.updated_ids.into_iter())
                .chain(parsed.article_updated_news_ids.unwrap_or_default().into_iter())
                .collect();
            changed.sort();
            changed.dedup();
            if changed.is_empty() {
                let _ = event_consumption_repo::mark(
                    &app,
                    EVT_NEWS_REFRESHED,
                    &parsed.batch_id,
                    CONSUMER,
                    ConsumptionStatus::Ignored,
                    None,
                    None,
                );
                return;
            }
            match news_buffer_repo::enqueue_pending(&app, &changed, &parsed.batch_id) {
                Ok(n) => {
                    let _ = event_consumption_repo::mark(
                        &app,
                        EVT_NEWS_REFRESHED,
                        &parsed.batch_id,
                        CONSUMER,
                        ConsumptionStatus::Consumed,
                        None,
                        None,
                    );
                    tracing::info!(
                        target = "agent_runtime.router",
                        added = n,
                        total_changed = changed.len(),
                        batch_id = %parsed.batch_id,
                        "news-refreshed 已入 buffer"
                    );
                }
                Err(e) => {
                    let _ = event_consumption_repo::mark(
                        &app,
                        EVT_NEWS_REFRESHED,
                        &parsed.batch_id,
                        CONSUMER,
                        ConsumptionStatus::Failed,
                        None,
                        Some(&e),
                    );
                    tracing::warn!(
                        target = "agent_runtime.router",
                        error = %e,
                        batch_id = %parsed.batch_id,
                        "news-refreshed 入 buffer 失败"
                    );
                }
            }
        });
    });
}

fn install_market_quotes_listener(app: AppHandle) {
    // 现有 pipeline::scheduler::account_snapshot_loop 已订阅该事件并 rebuild snapshot；
    // 这里只做事件可见性日志，避免和现有 listener 冲突。把 snapshot 重建 +
    // evaluate_account_triggers 编排移到本模块。
    let _ = &app;
    tracing::debug!(
        target = "agent_runtime.router",
        event = EVT_MARKET_QUOTES_REFRESHED,
        "listener 已委派 scheduler.account_snapshot_loop"
    );
}

fn install_account_triggered_listener(app: AppHandle) {
    let app_for_handler = app.clone();
    app.listen(EVT_ACCOUNT_TRIGGERED, move |event| {
        let app = app_for_handler.clone();
        let payload = event.payload().to_string();
        tauri::async_runtime::spawn(async move {
            let parsed: AccountTriggeredPayload = match serde_json::from_str(&payload) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        target = "agent_runtime.router",
                        error = %e,
                        "account-triggered payload 解析失败"
                    );
                    return;
                }
            };
            if parsed.trigger_id.is_empty() {
                return;
            }
            // spec §8 in-flight lock：agent.account_trigger:{trigger_id} 防 race
            use crate::infrastructure::agent_runtime::locks_ext;
            use crate::pipeline::agent_runtime::locks::InflightGuard;
            let lock_key = locks_ext::account_trigger_run(&parsed.trigger_id);
            let inflight_guard = match InflightGuard::acquire(lock_key) {
                Some(g) => g,
                None => return, // 同一 trigger 已有 run 在路由中
            };
            match event_consumption_repo::status_of(
                &app,
                EVT_ACCOUNT_TRIGGERED,
                &parsed.trigger_id,
                CONSUMER,
            ) {
                Ok(Some(ConsumptionStatus::Consumed | ConsumptionStatus::Ignored)) => return,
                _ => {}
            }
            let _ = event_consumption_repo::mark(
                &app,
                EVT_ACCOUNT_TRIGGERED,
                &parsed.trigger_id,
                CONSUMER,
                ConsumptionStatus::Processing,
                None,
                None,
            );

            // spec §2/§5.4：订单终态 trigger 若包含 orderId，Runtime 通过
            // agent_order_intent_index 反查找到原始 episode，把 episodeId 注入 run
            // 让 LLM 复盘时能直接关联到原决策。
            let originating_episode: Option<String> = parsed
                .order_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .and_then(|order_id| {
                    match trade_intents_repo::find_by_order(&app, order_id) {
                        Ok(Some((_intent_id, episode_id, _run_id))) => Some(episode_id),
                        Ok(None) => None,
                        Err(e) => {
                            tracing::warn!(
                                target = "agent_runtime.router",
                                error = %e,
                                order_id,
                                "find_by_order 失败，episode 反查跳过"
                            );
                            None
                        }
                    }
                });

            // 登记 account_trigger_response run；LLM loop 在下面 spawn，run 完成后
            // 在 on_success 回调里写 consumption.Consumed + mark_trigger_handled。
            let run_id = uuid::Uuid::new_v4().to_string();
            let run = AgentRun {
                run_id: run_id.clone(),
                profile_id: AgentRunProfileId::AccountTriggerResponse,
                episode_ids: originating_episode
                    .as_ref()
                    .map(|id| vec![id.clone()])
                    .unwrap_or_default(),
                trigger: AgentRunTrigger::AccountTrigger {
                    trigger_id: parsed.trigger_id.clone(),
                },
                provider: "deferred".into(),
                wire_format: "messages".into(),
                model: "deferred".into(),
                status: AgentRunStatus::Queued,
                started_at: Some(now()),
                ended_at: None,
                error: None,
            };
            match runs_repo::insert(&app, &run) {
                Ok(()) => {
                    // spec §5：登记 run 时只把 consumption 保持在 processing；
                    // 「仅启动 Agent run 不得标记 handled」。Consumed + mark_trigger_handled
                    // 只能在 run 完成消费后执行。
                    let _ = app.emit(
                        EVT_AGENT_RUN_STARTED,
                        serde_json::json!({
                            "runId": run_id,
                            "profileId": "account_trigger_response",
                            "triggerKind": "account_trigger",
                            "triggerId": parsed.trigger_id,
                        }),
                    );
                    tracing::info!(
                        target = "agent_runtime.router",
                        trigger_id = %parsed.trigger_id,
                        run_id = %run_id,
                        "account-triggered → queued + 启动 LLM loop"
                    );

                    // spawn 真实 LLM loop。完成后 mark consumed + Account.mark_trigger_handled。
                    let app_for_run = app.clone();
                    let trigger_id_for_run = parsed.trigger_id.clone();
                    let run_id_for_callback = run_id.clone();
                    let episode_hint = match &originating_episode {
                        Some(eid) => format!(
                            " 本 trigger 关联订单的原始 episodeId={eid}（spec §5.4 反查命中），\
                             record_decision_review 时使用 linked_episode source 引用该 episodeId 作为 evidence。"
                        ),
                        None => String::new(),
                    };
                    let prompt = format!(
                        "Account trigger 命中 trigger_id={}。{}请按纪律：1) 用 fetch_account 拉 \
                         当前账户 / 持仓 / 触发详情；2) 判断是否需要平仓 / 调止损 / 撤单；\
                         3) 形成判断必须 record_decision_episode；4) 必要时 operate_account \
                         走同一 episodeId；5) 完成后用 record_decision_review 落复盘。",
                        parsed.trigger_id, episode_hint
                    );
                    tauri::async_runtime::spawn(super::background_run::run_background_loop(
                        app_for_run,
                        run_id,
                        AgentRunProfileId::AccountTriggerResponse,
                        "account_trigger",
                        prompt,
                        move |inner_app| {
                            // inflight_guard 持有到这里 drop，保证 lock 覆盖整个 run 生命周期
                            let _guard = inflight_guard;
                            // spec §5：只有 run 完成消费才能 mark consumed + mark_trigger_handled
                            let _ = event_consumption_repo::mark(
                                inner_app,
                                EVT_ACCOUNT_TRIGGERED,
                                &trigger_id_for_run,
                                CONSUMER,
                                ConsumptionStatus::Consumed,
                                Some(&run_id_for_callback),
                                None,
                            );
                            let svc = crate::pipeline::account::AccountService::new(
                                inner_app.clone(),
                            );
                            if let Err(e) = svc.mark_trigger_handled(&trigger_id_for_run) {
                                tracing::warn!(
                                    target = "agent_runtime.router",
                                    error = %e,
                                    trigger_id = %trigger_id_for_run,
                                    "mark_trigger_handled 失败"
                                );
                            }
                        },
                    ));
                }
                Err(e) => {
                    let _ = event_consumption_repo::mark(
                        &app,
                        EVT_ACCOUNT_TRIGGERED,
                        &parsed.trigger_id,
                        CONSUMER,
                        ConsumptionStatus::Failed,
                        None,
                        Some(&e),
                    );
                    tracing::warn!(
                        target = "agent_runtime.router",
                        trigger_id = %parsed.trigger_id,
                        error = %e,
                        "account-triggered run 登记失败"
                    );
                }
            }
        });
    });
}
