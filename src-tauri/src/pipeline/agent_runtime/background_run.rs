//! 后台 Agent run dispatcher（news_analysis / account_trigger_response 等）。
//!
//! 对齐 spec `agent-runtime-module.md §5`：
//! - 拿 trigger 描述 + packet
//! - 复用 Agent Infra `run_agent` loop（带 7-tool registry）
//! - 完成后更新 AgentRun status；按 trigger 类型调用 News.mark_consumed /
//!   Account.mark_trigger_handled
//!
//! 与 chat 流的差异：不写 chat_messages 表、不打 chat-message-appended 事件、
//! 不需要 history compact / 边界 summary —— 后台 run 是一次性短任务。

use std::sync::Arc;

use serde_json::json;
use tauri::AppHandle;
use tokio::sync::mpsc;

use crate::domain::agent::types::{
    AgentEvent, AgentOptions, AgentRequest, Block, ContextBudget, Message, PipelineKind, Role,
    SystemBlock,
};
use crate::domain::agent_runtime::runs::{AgentRunProfileId, AgentRunStatus};
use crate::infrastructure::agent_runtime::runs_repo;
use crate::pipeline::agent::config::{build_provider_for_channel, read_agent_config};
use crate::pipeline::agent::prompt::AGENT_IDENTITY;
use crate::pipeline::agent::run_agent;
use crate::pipeline::agent::tools::ToolContext;

/// 启动一次后台 Agent run。spawn 在调用方负责。
///
/// `trigger_prompt`：合成的 user 消息（中文），描述本次 trigger / 待分析 batch。
/// `on_success`：run 完成（completed / cancelled）时调用，用于
/// `News.mark_consumed` / `Account.mark_trigger_handled` 等收尾动作。
pub async fn run_background_loop(
    app: AppHandle,
    run_id: String,
    profile: AgentRunProfileId,
    trigger_kind: &'static str,
    trigger_prompt: String,
    on_success: impl FnOnce(&AppHandle) + Send + 'static,
) {
    // 占位：调用方可通过 `move |inner| { ... }` 在闭包内捕获 InflightGuard，
    // run 完成时一起 drop。当前 router/account_trigger 直接把 guard 移进
    // on_success 闭包，无需额外参数。
    run_background_loop_impl(app, run_id, profile, trigger_kind, trigger_prompt, on_success).await;
}

async fn run_background_loop_impl(
    app: AppHandle,
    run_id: String,
    profile: AgentRunProfileId,
    trigger_kind: &'static str,
    trigger_prompt: String,
    on_success: impl FnOnce(&AppHandle) + Send + 'static,
) {
    // spec §9 race fix：spawn 前如果 cancel flag 已 set（cancel 命令先于 spawn 到达），
    // 直接落 Cancelled 不再启动 run_inner，避免 status 从 Cancelled 被覆盖回 Running。
    if crate::infrastructure::agent_runtime::cancellation::is_cancelled_by_id(&run_id) {
        let _ = runs_repo::update_status(
            &app,
            &run_id,
            AgentRunStatus::Cancelled,
            Some("cancelled_before_start"),
            Some(&chrono::Utc::now().to_rfc3339()),
        );
        crate::pipeline::agent_runtime::run_scope::remove(&run_id);
        return;
    }
    let result = run_inner(&app, &run_id, profile, trigger_kind, trigger_prompt).await;
    let cancelled =
        crate::infrastructure::agent_runtime::cancellation::is_cancelled_by_id(&run_id);
    // 清理 run scope（spec EvidenceRef 校验只在 run 生命周期内有效）
    crate::pipeline::agent_runtime::run_scope::remove(&run_id);
    match result {
        Ok(_) if cancelled => {
            let _ = runs_repo::update_status(
                &app,
                &run_id,
                AgentRunStatus::Cancelled,
                Some("cancelled_by_user"),
                Some(&chrono::Utc::now().to_rfc3339()),
            );
            tracing::info!(
                target = "agent_runtime.background_run",
                run_id = %run_id,
                "background agent run cancelled"
            );
        }
        Ok(_) => {
            let _ = runs_repo::update_status(
                &app,
                &run_id,
                AgentRunStatus::Completed,
                None,
                Some(&chrono::Utc::now().to_rfc3339()),
            );
            tauri::async_runtime::spawn(async move {
                on_success(&app);
            });
        }
        Err(e) => {
            tracing::warn!(
                target = "agent_runtime.background_run",
                error = %e,
                run_id = %run_id,
                "background agent run 失败"
            );
            let _ = runs_repo::update_status(
                &app,
                &run_id,
                AgentRunStatus::Failed,
                Some(&e),
                Some(&chrono::Utc::now().to_rfc3339()),
            );
        }
    }
}

fn update_agent_run_channel(
    app: &AppHandle,
    run_id: &str,
    provider: &str,
    wire_format: &str,
    model: &str,
) -> Result<(), String> {
    use crate::infrastructure::db::{migrate, open_database};
    let c = open_database(app)?;
    migrate(&c)?;
    c.execute(
        "update agent_runs
         set provider = ?2, wire_format = ?3, model = ?4, updated_at = ?5
         where run_id = ?1",
        rusqlite::params![
            run_id,
            provider,
            wire_format,
            model,
            crate::infrastructure::db::helpers::now()
        ],
    )
    .map_err(|e| format!("update agent_run channel/model 失败：{e}"))?;
    Ok(())
}

async fn run_inner(
    app: &AppHandle,
    run_id: &str,
    profile: AgentRunProfileId,
    trigger_kind: &'static str,
    trigger_prompt: String,
) -> Result<(), String> {
    // 状态推进：queued → running
    runs_repo::update_status(
        app,
        run_id,
        AgentRunStatus::Running,
        None,
        Some(&chrono::Utc::now().to_rfc3339()),
    )?;

    let cfg = read_agent_config(app);
    cfg.ensure_ready()?;
    let (chan_ref, model_ref) =
        cfg.resolve_pipeline(PipelineKind::Chat).map_err(|e| e)?;
    let channel = chan_ref.clone();
    let model = model_ref.to_string();
    // spec §2「单个 run 使用单个 provider channel / model」：真实 channel/model 写回 agent_runs
    let _ = update_agent_run_channel(
        app,
        &run_id,
        &channel.name,
        channel.wire_format.as_str(),
        &model,
    );

    // 构 packet → system context summary
    let packet = super::packet::build(app, run_id, profile, trigger_kind)
        .map_err(|e| format!("packet build 失败：{e}"))?;
    let packet_summary = packet.to_summary_text();

    let system_blocks = vec![
        SystemBlock {
            text: AGENT_IDENTITY.to_string(),
            cache_control: false,
        },
        SystemBlock {
            text: format!(
                "你正在执行一次 {trigger_kind} 后台 Agent run。\n\n\
                 必须遵守的纪律：\n\
                 - 形成投资判断（含 no_action / 加入自选 / 调仓 / 平仓）必须先调用 \
                 record_decision_episode 落 episode；交易动作必须使用同一 run 内已 \
                 accept 的 episodeId 调 operate_account。\n\
                 - 行情 / 账户 / 资讯通过 fetch_quotes / fetch_account / fetch_news 实时拉。\n\
                 - 完成审计后用自然语言简短总结本次判断结论。",
            ),
            cache_control: false,
        },
        SystemBlock {
            text: packet_summary,
            cache_control: true,
        },
    ];

    let user_msg = Message {
        role: Role::User,
        content: vec![Block::Text {
            text: trigger_prompt,
            cache_control: false,
        }],
    };

    // 构建 tool registry —— 按 profile 的 allowedTools + allowTradingWrite 过滤。
    // spec §2 / §4 / §12 验收：「Infra 不默认暴露所有工具」「禁止交易写的 profile
    // 不能调用 operate_account」。`scheduled_review` 的 trading_write 由 KV 控制。
    let runtime_cfg = crate::infrastructure::agent_runtime::settings::load(app);
    let trading_write_override = match profile {
        AgentRunProfileId::ScheduledReview => {
            Some(runtime_cfg.scheduled_review_allow_trading_write)
        }
        _ => None,
    };
    let profile_str = match profile {
        AgentRunProfileId::UserChat => "user_chat",
        AgentRunProfileId::NewsAnalysis => "news_analysis",
        AgentRunProfileId::AccountTriggerResponse => "account_trigger_response",
        AgentRunProfileId::ScheduledReview => "scheduled_review",
        AgentRunProfileId::ManualReplay => "manual_replay",
    };
    let registry = Arc::new(
        crate::pipeline::agent::tools::build_registry_for(app, profile_str, trading_write_override)
            .ok_or_else(|| "tool registry factory not installed".to_string())?,
    );
    let tools = registry.to_tool_defs(true);

    let req = AgentRequest {
        system: system_blocks,
        tools,
        messages: vec![user_msg],
        options: AgentOptions {
            model,
            max_tokens: 4096,
            temperature: Some(0.5),
            top_p: None,
            thinking: channel.thinking_config(),
            effort: channel.default_effort,
            max_turns: cfg.agent.max_turns_per_run,
            stop_sequences: vec![],
            tool_timeout_secs: Some(cfg.agent.tool_timeout_secs),
        },
        budget: ContextBudget {
            // spec §8 runtime settings keys：优先用 runtime KV，fallback 到 agent config
            soft_limit_tokens: if runtime_cfg.context_soft_limit_tokens > 0 {
                runtime_cfg.context_soft_limit_tokens as u32
            } else {
                cfg.agent.context_soft_limit_tokens
            },
            hard_limit_tokens: if runtime_cfg.context_hard_limit_tokens > 0 {
                runtime_cfg.context_hard_limit_tokens as u32
            } else {
                cfg.agent.context_hard_limit_tokens
            },
            compact_keep_last_n: cfg.agent.compact_keep_last_n_turns,
            max_search_calls: cfg.agent.max_search_calls_per_run,
        },
        trigger_message_id: None,
        pipeline: PipelineKind::Chat,
    };

    let provider = build_provider_for_channel(&channel)
        .map_err(|e| format!("构建 provider 失败：{e}"))?;

    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    // collector：吞掉所有事件并 emit 给前端，避免 channel 满
    let app_for_emit = app.clone();
    let emitter = tokio::spawn(async move {
        use tauri::Emitter;
        while let Some(ev) = rx.recv().await {
            let _ = app_for_emit.emit(crate::pipeline::agent::observer::AGENT_EVENT, &ev);
        }
    });

    let ctx = ToolContext {
        run_id: run_id.to_string(),
        app: Some(app.clone()),
        tool_call_id: None,
    };
    let _summary = run_agent(provider, None, registry, req, ctx, tx)
        .await
        .map_err(|e| format!("agent loop 错误：{e}"))?;
    drop(emitter);

    use tauri::Emitter;
    let _ = app.emit(
        crate::pipeline::agent_runtime::router::EVT_AGENT_RUN_FINISHED,
        json!({ "runId": run_id, "triggerKind": trigger_kind }),
    );
    Ok(())
}
