//! Pipeline 共用的 agent 调用 helper——封装 chat 之外两个 pipeline（briefing/review）
//! "把一段大 prompt 喂进去 → 拿最终文本回来 parse" 的固定模式。
//!
//! Chat pipeline 不走这里，因为它需要流式 TextDelta 实时 emit + 自己累积 + 写
//! assistant 消息。Briefing/Review 等 run 跑完才一次性 parse + 落盘。

use crate::agent::config::{
    build_provider_for_channel, read_agent_config, AgentConfig, Channel, ProviderKind,
};
use crate::agent::observer;
use crate::agent::run_agent;
use crate::agent::tools::{build_readonly_registry, ToolContext};
use crate::agent::types::{
    AgentEvent, AgentOptions, AgentRequest, Block, ContextBudget, EffortLevel, Message,
    PipelineKind, Role, ServerSideTool, StopReason, SystemBlock, ThinkingConfig, ToolDef,
};
use crate::prompt::AGENT_IDENTITY;
use std::sync::Arc;
use tauri::AppHandle;
use tokio::sync::mpsc;

/// 跑一次 agent run，等其完成，把所有 TextDelta 拼成最终文本返回。
/// briefing/review 的标准用法——它们的 prompt 要求 agent 输出 JSON，调用方 parse。
///
/// 中途的 AgentEvent 会被 emit 给前端（让 UI 看到工具调用进度），
/// 不会卡住主流程。
///
/// 返回 final_text；run summary（token / turns / 工具调用数）已通过
/// observer::finalize 落 agent_runs 表，调用方一般不需要程序化访问。
/// Pipeline-level thinking override：briefing/review 想强制启用 / 关闭某种 thinking
/// 模式时传这个；`None` 表示沿用 channel 的默认。
///
/// 单独表达"override 关闭" vs "沿用 channel"——前者用 `Some(None)`（=用户配了
/// Adaptive 但 pipeline 强制关），后者用 `None`（=沿用 channel 的 thinking_config）。
pub type ThinkingOverride = Option<Option<ThinkingConfig>>;

pub async fn run_agent_text(
    app: &AppHandle,
    pipeline: PipelineKind,
    user_prompt: String,
    thinking_override: ThinkingOverride,
    effort_override: Option<EffortLevel>,
    trigger_message_id: Option<String>,
) -> Result<String, String> {
    let cfg = read_agent_config(app);
    cfg.ensure_ready()?;
    // 解析这条 pipeline 的 (channel, model)——run_agent_text 拿到具体渠道才能
    // build_provider，不再有"全局 provider"概念。
    let (channel, model) = cfg.resolve_pipeline(pipeline).map_err(|e| e.to_string())?;
    let channel = channel.clone(); // 之后 await 跨边界要 owned
    let model = model.to_string();

    // briefing / review 用只读工具集——memory 增量由 prompt 协议（JSON 字段）传回，
    // 不让 agent 调 update_memory 工具，避免和 pipeline 的 merge 路径互相覆盖。
    let registry = Arc::new(build_readonly_registry(app));
    let req = build_text_output_request(
        &cfg,
        &channel,
        &model,
        pipeline,
        user_prompt,
        &registry,
        thinking_override,
        effort_override,
        trigger_message_id.clone(),
    );

    let run_id = uuid::Uuid::new_v4().to_string();
    observer::start_run(
        app,
        &run_id,
        pipeline,
        channel.wire_format.as_str(),
        &model,
        trigger_message_id.as_deref(),
    )?;

    let provider =
        build_provider_for_channel(&channel).map_err(|e| format!("构建 provider 失败：{e}"))?;

    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let app_for_collector = app.clone();
    let collector_run_id = run_id.clone();
    let collector = tokio::spawn(async move {
        use tauri::Emitter;
        let mut answer = String::new();
        let mut acc = observer::TurnAccumulator::new(collector_run_id);
        while let Some(ev) = rx.recv().await {
            let _ = app_for_collector.emit(observer::AGENT_EVENT, &ev);
            if let Some(rec) = acc.consume(&ev) {
                if let Err(e) = observer::record_turn(
                    &app_for_collector,
                    acc.run_id(),
                    rec.turn,
                    &rec.started_at,
                    &rec.ended_at,
                    rec.stop_reason,
                    rec.input_tokens,
                    rec.output_tokens,
                    rec.cache_read_tokens,
                    rec.local_tool_calls,
                    rec.server_tool_calls,
                    None,
                ) {
                    tracing::warn!(error = %e, run_id = acc.run_id(), turn = rec.turn, "落 agent_run_turns 失败");
                }
            }
            if let AgentEvent::TextDelta { delta, .. } = &ev {
                answer.push_str(delta);
            }
        }
        answer
    });

    let ctx = ToolContext {
        run_id: run_id.clone(),
    };
    // briefing/review pipeline 输入边界已经 capped（news buffer / 单 record），
    // 不需要 LLM 摘要。压缩交给规则化 MicroClear/Drop 即可。
    let summary_result = run_agent(provider, None, registry, req, ctx, tx).await;
    let collected_text = collector
        .await
        .map_err(|e| format!("collector join 失败：{e}"))?;

    let summary = match summary_result {
        Ok(s) => s,
        Err(e) => {
            let err_msg = format!("agent run 失败：{e}");
            let _ = observer::finalize_failure(app, &run_id, &err_msg);
            return Err(err_msg);
        }
    };
    // briefing/review 的输出协议是 JSON——max_tokens 截断会让 JSON 不完整，
    // 后续 parse 一定挂。在这里报清楚错误，比让 parse_briefing 抛 "expected `,` or `}`"
    // 之类的 serde 错好得多。pipeline 调用方拿到字符串错误后会写一条 system 消息。
    if matches!(summary.stop_reason, StopReason::MaxTokens) {
        let err_msg = format!(
            "agent 输出被 max_tokens 截断（output_tokens={}），无法解析为完整 JSON。建议增大 max_tokens 或简化输入上下文。",
            summary.total_output_tokens
        );
        let _ = observer::finalize(app, &summary, Some(&err_msg));
        return Err(err_msg);
    }
    let _ = observer::finalize(app, &summary, None);

    // Briefing/review parse the model's final JSON. If the agent used tools,
    // earlier assistant turns may contain visible pre-tool text; do not mix that
    // into the parser input.
    let final_text = summary
        .final_message
        .as_ref()
        .and_then(message_text)
        .unwrap_or(collected_text);

    Ok(final_text)
}

fn message_text(message: &Message) -> Option<String> {
    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            Block::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn build_text_output_request(
    cfg: &AgentConfig,
    channel: &Channel,
    model: &str,
    pipeline: PipelineKind,
    user_prompt: String,
    registry: &Arc<crate::agent::tools::ToolRegistry>,
    thinking_override: ThinkingOverride,
    effort_override: Option<EffortLevel>,
    trigger_message_id: Option<String>,
) -> AgentRequest {
    let mut tools = registry.to_tool_defs(true);
    // 是否启用 server-side web_search——按这个 pipeline 用的渠道的 wire_format + 开关
    let want_web_search = match channel.wire_format {
        ProviderKind::Anthropic => channel.enable_native_web_search,
        ProviderKind::OpenAIResponses => channel.enable_web_search,
        ProviderKind::OpenAIChatCompletions => false,
    };
    if want_web_search {
        tools.push(ToolDef::ServerSide(ServerSideTool::AnthropicWebSearch {
            name: "web_search".into(),
            max_uses: Some(cfg.agent.max_search_calls_per_run),
            allowed_domains: vec![],
            blocked_domains: vec![],
        }));
    }
    // thinking：pipeline override 优先，否则用 channel 配置的模式
    let thinking = thinking_override.unwrap_or_else(|| channel.thinking_config());
    // effort：pipeline override 优先，否则用 channel 默认（None 时不传字段）
    let effort = effort_override.or(channel.default_effort);
    AgentRequest {
        system: vec![SystemBlock {
            text: AGENT_IDENTITY.to_string(),
            cache_control: true,
        }],
        tools,
        messages: vec![Message {
            role: Role::User,
            content: vec![Block::Text {
                text: user_prompt,
                cache_control: false,
            }],
        }],
        options: AgentOptions {
            model: model.to_string(),
            max_tokens: 8192,
            temperature: Some(0.3), // briefing/review 偏稳定
            top_p: None,
            thinking,
            effort,
            max_turns: cfg.agent.max_turns_per_run,
            stop_sequences: vec![],
            tool_timeout_secs: Some(cfg.agent.tool_timeout_secs),
        },
        budget: ContextBudget {
            soft_limit_tokens: cfg.agent.context_soft_limit_tokens,
            hard_limit_tokens: cfg.agent.context_hard_limit_tokens,
            compact_keep_last_n: cfg.agent.compact_keep_last_n_turns,
            max_search_calls: cfg.agent.max_search_calls_per_run,
        },
        trigger_message_id,
        pipeline,
    }
}
