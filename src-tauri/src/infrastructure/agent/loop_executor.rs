//! Canonical Agent loop 编排（infra 内部 use case）。
//!
//! Spec: docs/design/agent-infra-module.md §3 Agent Loop，§4 Reactive retry，§5 Infra Loop API
//!
//! 行为：
//! 1. emit `run_start`
//! 2. 调 provider stream（trait `ProviderStream`，便于注入 fake provider 测试）
//! 3. FIX 1（spec §2/§3）：`SkillCallParser` 现在跑在 **provider 内部**，clean `TextDelta`
//!    由 provider 实时 emit（XML 抑制）。loop 只消费 `outcome.skill_events`：
//!    - `UseSkill` → emit `skill_start` → `SkillRegistry::dispatch_skill_call` →
//!      emit `skill_end` → 缓存 `<skill_result>` 文本
//!    - `ParseError` → 把 `<skill_error code="parse_error">` 加到本轮回写文本
//!    loop **不再** 自己跑 parser、**不再** re-emit `TextDelta`（避免重复）。
//!    消息历史用 `outcome.text`（raw，含 `<use_skill>` XML）回写。
//! 4. turn 结束：
//!    - 若本 turn 触发了 ≥ 1 次 dispatch：构造新一轮 user message（按出现顺序串联
//!      `<skill_result>` / `<skill_error>`），继续 loop。
//!    - 否则 finalize：emit usage / done。
//! 5. Reactive retry：catch `ProviderContextTooLong` → `compact_context(ReactiveRetry)`
//!    → 重发同一 turn（最多 1 次）→ 仍失败则 `stop_reason = context_limit`。

use crate::domain::agent::{
    AgentEvent, AgentMessage, AgentMessageBlock, AgentMessageRole, AgentRunRequest,
    AgentStopReason, ContextBundle, ContextContent, ContextPart, ContextPartKind, RunSummary,
};
use crate::domain::shared::ErrorCode;
use crate::infrastructure::agent::context_compaction::{compact_context, CompactPolicy};
use crate::infrastructure::agent::skill_parser::ParserEvent;
use crate::infrastructure::agent::skill_registry::{DispatchError, SkillRegistry};
use crate::infrastructure::agent::system_prompt::build_system_prompt;
use chrono::Utc;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use uuid::Uuid;

/// 一次 provider stream 的拉取结果。
///
/// 本 trait 抽象掉具体 SSE 解码细节，让 loop 测试可以注入 mock。
///
/// FIX 1（spec §2/§3）：`SkillCallParser` 现在跑在 **streaming provider 内部**，
/// 所以 provider 在流式过程中已经 emit 过 clean（XML-suppressed）的 `TextDelta`。
/// `skill_events` 携带本 turn 解析出的 `UseSkill` / `ParseError`（按出现顺序），供
/// loop dispatch；其中**不包含** `TextDelta`（已由 provider emit，loop 不再 re-emit）。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderTurnOutcome {
    /// provider 输出的 **raw** chat text（含 `<use_skill>` XML 标签），用于回写消息历史。
    pub text: String,
    pub usage_input: u32,
    pub usage_output: u32,
    /// provider 原始 stop reason（adapter 归一化后值）。
    pub stop_reason: AgentStopReason,
    /// 本 turn 解析出的 skill 事件（`UseSkill` / `ParseError`，按出现顺序）。
    /// TextDelta 已由 provider emit，**不**出现在此 vec。
    pub skill_events: Vec<ParserEvent>,
}

/// Provider stream 抽象。Loop executor 在每个 turn 调用一次 `next_turn`。
#[async_trait::async_trait]
pub trait ProviderStream: Send + Sync {
    async fn next_turn(
        &mut self,
        messages: &[AgentMessage],
        context: &ContextBundle,
        event_tx: &Sender<AgentEvent>,
        run_id: &str,
    ) -> Result<ProviderTurnOutcome, LoopError>;
}

#[derive(Debug, thiserror::Error)]
pub enum LoopError {
    #[error("provider error: {0}")]
    Provider(String),
    /// Spec §4: provider 返回 context-too-long（HTTP 400 或等价 error code）。
    #[error("provider context too long")]
    ProviderContextTooLong,
    #[error("event channel closed")]
    EventChannelClosed,
}

/// 执行一次 Agent loop。
///
/// Spec §5：`run_agent_loop(request, registry, context, event_tx) -> RunSummary`
pub async fn run_agent_loop(
    request: AgentRunRequest,
    registry: Arc<SkillRegistry>,
    mut context: ContextBundle,
    mut provider: Box<dyn ProviderStream>,
    event_tx: Sender<AgentEvent>,
) -> Result<RunSummary, LoopError> {
    let run_id = request.run_id.clone();

    // Spec §2 line 209-210, §5 line 533: 每次 Agent loop 启动时,Infra 用 `SystemPromptBuilder`
    // 把 enabled `SkillSpec` 集合编译成 system prompt 前缀,自动 prepend 到 ContextBundle.systemParts。
    // Runtime 不需要手动塞;这里在 emit run_start 之前一次性 prepend。
    prepend_skill_list_to_system_parts(&mut context, &registry);

    send_event(
        &event_tx,
        AgentEvent::RunStart {
            run_id: run_id.clone(),
            trigger: request.trigger.clone(),
            model: request.channel.model.clone(),
        },
    )
    .await?;

    let mut messages: Vec<AgentMessage> = request.seed_messages.clone();
    let mut skill_call_ids: Vec<String> = Vec::new();
    let mut usage_input: u32 = 0;
    let mut usage_output: u32 = 0;

    let mut turn: u32 = 0;
    let mut reactive_retry_used = false;
    let stop_reason: AgentStopReason;
    'outer: loop {
        if turn >= request.max_turns {
            stop_reason = AgentStopReason::MaxTurns;
            break;
        }
        turn += 1;

        // ---- provider call with reactive retry ----
        let outcome = match provider
            .next_turn(&messages, &context, &event_tx, &run_id)
            .await
        {
            Ok(out) => out,
            Err(LoopError::ProviderContextTooLong) => {
                if reactive_retry_used {
                    // Spec §4: 最多 1 次 reactive retry。第二次仍失败 → fail closed。
                    send_event(
                        &event_tx,
                        AgentEvent::Error {
                            run_id: run_id.clone(),
                            code: ErrorCode::ProviderContextTooLong,
                            message: "provider context too long after reactive retry".into(),
                        },
                    )
                    .await?;
                    stop_reason = AgentStopReason::ContextLimit;
                    break;
                }
                // Compact + emit compacted event, then retry same turn.
                reactive_retry_used = true;
                let (new_ctx, dropped) = compact_context(
                    context.clone(),
                    CompactPolicy::ReactiveRetry,
                    crate::domain::agent::ContextWindowLimits::default(),
                );
                context = new_ctx;
                send_event(
                    &event_tx,
                    AgentEvent::Compacted {
                        run_id: run_id.clone(),
                        tier: crate::domain::agent::CompactedTier::ReactiveRetry,
                        dropped_messages: dropped,
                        estimated_tokens_saved: None,
                    },
                )
                .await?;
                // Rewind turn counter (this turn didn't actually progress) and retry.
                turn -= 1;
                continue 'outer;
            }
            Err(e) => return Err(e),
        };
        usage_input = usage_input.saturating_add(outcome.usage_input);
        usage_output = usage_output.saturating_add(outcome.usage_output);

        // ---- FIX 1: provider already ran SkillCallParser and emitted clean TextDelta.
        // Loop consumes only the parsed skill events; it does NOT re-feed text and does
        // NOT re-emit TextDelta. Assistant message history uses outcome.text (raw XML).
        let mut skill_results_for_next_turn: Vec<String> = Vec::new();
        let mut any_dispatch = false;

        for ev in outcome.skill_events.iter().cloned() {
            match ev {
                ParserEvent::TextDelta(_) => {
                    // Provider already emitted clean TextDelta; loop ignores any here.
                }
                ParserEvent::UseSkill { name, input } => {
                    any_dispatch = true;
                    let call_id = SkillRegistry::new_skill_call_id();

                    send_event(
                        &event_tx,
                        AgentEvent::SkillStart {
                            run_id: run_id.clone(),
                            skill_call_id: call_id.clone(),
                            name: name.clone(),
                            input_summary: input.clone(),
                        },
                    )
                    .await?;

                    let dispatch_res = registry
                        .dispatch_skill_call(&run_id, call_id.clone(), &name, input.clone())
                        .await;

                    let (out_summary, is_error, duration_ms, used_call_id, err_code) =
                        match dispatch_res {
                            Ok(r) => (
                                r.output_summary,
                                r.is_error,
                                r.duration_ms,
                                r.skill_call_id,
                                r.error_code,
                            ),
                            Err(DispatchError::NotRegistered(_)) => {
                                let summary = serde_json::json!({
                                    "message": format!("skill '{}' not registered", name),
                                });
                                (summary, true, 0u64, call_id.clone(), Some(ErrorCode::InvalidInput))
                            }
                            Err(DispatchError::InvalidInput(msg)) => {
                                let summary = serde_json::json!({"message": msg});
                                (summary, true, 0u64, call_id.clone(), Some(ErrorCode::InvalidInput))
                            }
                            Err(e) => {
                                let summary = serde_json::json!({"message": e.to_string()});
                                (summary, true, 0u64, call_id.clone(), Some(ErrorCode::ParseError))
                            }
                        };

                    skill_call_ids.push(used_call_id.clone());
                    send_event(
                        &event_tx,
                        AgentEvent::SkillEnd {
                            run_id: run_id.clone(),
                            skill_call_id: used_call_id.clone(),
                            name: name.clone(),
                            output_summary: out_summary.clone(),
                            is_error,
                            duration_ms,
                        },
                    )
                    .await?;

                    // Format <skill_result> / <skill_error> for next turn user message.
                    let payload_str = serde_json::to_string(&out_summary)
                        .unwrap_or_else(|_| "{}".into());
                    if is_error {
                        let code_str =
                            err_code.map(error_code_str).unwrap_or("parse_error");
                        skill_results_for_next_turn.push(format!(
                            r#"<skill_error name="{}" call_id="{}" code="{}">{}</skill_error>"#,
                            name, used_call_id, code_str, payload_str
                        ));
                    } else {
                        skill_results_for_next_turn.push(format!(
                            r#"<skill_result name="{}" call_id="{}">{}</skill_result>"#,
                            name, used_call_id, payload_str
                        ));
                    }
                }
                ParserEvent::ParseError { reason, partial: _ } => {
                    // Spec §2: 标签嵌套不合法 / JSON parse 错 → 返回 <skill_error code="parse_error">。
                    // raw partial 已包含在 outcome.text 中（assistant 历史从 outcome.text 回写）。
                    any_dispatch = true;
                    let call_id = SkillRegistry::new_skill_call_id();
                    skill_call_ids.push(call_id.clone());
                    let payload = serde_json::json!({"message": reason});
                    let payload_str =
                        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
                    skill_results_for_next_turn.push(format!(
                        r#"<skill_error name="_parser" call_id="{}" code="parse_error">{}</skill_error>"#,
                        call_id, payload_str
                    ));
                }
            }
        }

        // Persist assistant message (raw text, incl <use_skill> XML) for LLM history.
        if !outcome.text.is_empty() {
            messages.push(AgentMessage {
                message_id: format!("am-{}", Uuid::new_v4()),
                run_id: Some(run_id.clone()),
                role: AgentMessageRole::Assistant,
                blocks: vec![AgentMessageBlock::Text {
                    text: outcome.text.clone(),
                }],
                created_at: Utc::now(),
            });
        }

        if !any_dispatch {
            // No skill use → finalize.
            stop_reason = match outcome.stop_reason {
                AgentStopReason::MaxTurns => AgentStopReason::MaxTurns,
                AgentStopReason::ProviderStop => AgentStopReason::ProviderStop,
                _ => AgentStopReason::Completed,
            };
            break;
        }

        // Append next-turn user message carrying the <skill_result>s.
        let user_text = skill_results_for_next_turn.join("\n");
        messages.push(AgentMessage {
            message_id: format!("am-{}", Uuid::new_v4()),
            run_id: Some(run_id.clone()),
            role: AgentMessageRole::User,
            blocks: vec![AgentMessageBlock::Text { text: user_text }],
            created_at: Utc::now(),
        });
        // Continue to next turn.
    }

    // FIX 6: usage event semantics.
    // The provider emits a *per-turn* `AgentEvent::Usage` inside `next_turn` (one per
    // provider round-trip). Here the loop emits the *cumulative run total* exactly ONCE,
    // just before `Done`. Same variant, but disambiguated by position: the final Usage
    // immediately preceding Done is always the run total; any earlier Usage is per-turn.
    // (We keep the public AgentEvent shape unchanged; consumers that need the run total
    // can read the last Usage before Done, which also matches RunSummary.)
    send_event(
        &event_tx,
        AgentEvent::Usage {
            run_id: run_id.clone(),
            input_tokens: usage_input,
            output_tokens: usage_output,
            cache_read_tokens: None,
            cache_write_tokens: None,
        },
    )
    .await?;
    send_event(
        &event_tx,
        AgentEvent::Done {
            run_id: run_id.clone(),
            stop_reason,
            turns: turn,
        },
    )
    .await?;

    Ok(RunSummary {
        run_id,
        stop_reason,
        turns: turn,
        input_tokens: usage_input,
        output_tokens: usage_output,
        cache_read_tokens: None,
        cache_write_tokens: None,
        skill_call_ids,
    })
}

fn error_code_str(c: ErrorCode) -> &'static str {
    match c {
        ErrorCode::InvalidInput => "invalid_input",
        ErrorCode::NotFound => "not_found",
        ErrorCode::ProviderUnavailable => "provider_unavailable",
        ErrorCode::RateLimited => "rate_limited",
        ErrorCode::DbError => "db_error",
        ErrorCode::ParseError => "parse_error",
        ErrorCode::QuoteMissing => "quote_missing",
        ErrorCode::QuoteStale => "quote_stale",
        ErrorCode::QuotePriceMissing => "quote_price_missing",
        ErrorCode::DepthMissing => "depth_missing",
        ErrorCode::OutsideTradingSession => "outside_trading_session",
        ErrorCode::InstrumentNotTradable => "instrument_not_tradable",
        ErrorCode::InstrumentSuspended => "instrument_suspended",
        ErrorCode::LimitUpDownBlocked => "limit_up_down_blocked",
        ErrorCode::InsufficientCash => "insufficient_cash",
        ErrorCode::InsufficientSellableQuantity => "insufficient_sellable_quantity",
        ErrorCode::InvalidLotSize => "invalid_lot_size",
        ErrorCode::OrderNotPending => "order_not_pending",
        ErrorCode::RiskLimitExceeded => "risk_limit_exceeded",
        ErrorCode::StrategyRequired => "strategy_required",
        ErrorCode::DuplicateEvent => "duplicate_event",
        ErrorCode::VersionConflict => "version_conflict",
        ErrorCode::ArticleExtractFailed => "article_extract_failed",
        ErrorCode::ToolTimeout => "tool_timeout",
        ErrorCode::ProviderContextTooLong => "provider_context_too_long",
    }
}

async fn send_event(tx: &Sender<AgentEvent>, e: AgentEvent) -> Result<(), LoopError> {
    tx.send(e).await.map_err(|_| LoopError::EventChannelClosed)
}

/// Spec §2 System Prompt Skill 清单 / §5 line 533:
/// 用 `SystemPromptBuilder` 把已注册的 SkillSpec 列表编译成 markdown 前缀,
/// 作为 `kind = "system"` 的 ContextPart 自动 prepend 到 `systemParts`(droppable=false)。
///
/// 注意:protocol_preamble 即便没有任何 skill 也注入,保证模型始终知道 `<use_skill>` 文本协议。
fn prepend_skill_list_to_system_parts(context: &mut ContextBundle, registry: &SkillRegistry) {
    let skills = registry.list_skills();
    let prompt = build_system_prompt(&skills, "");
    if prompt.is_empty() {
        return;
    }
    let token_estimate = ((prompt.chars().count() + 3) / 4) as u32;
    let part = ContextPart {
        kind: ContextPartKind::System,
        content: ContextContent::Text(prompt),
        freshness: None,
        token_estimate: Some(token_estimate),
        droppable: false,
    };
    context.system_parts.insert(0, part);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::{
        ProviderChannel, SideEffect, SkillSpec, WireFormat,
    };
    use crate::infrastructure::agent::skill_parser::SkillCallParser;
    use crate::infrastructure::agent::skill_registry::{
        FnSkillHandler, SkillHandler, SkillHandlerFuture, SkillHandlerOutput, SkillInvocation,
    };
    use serde_json::json;
    use tokio::sync::mpsc;

    fn channel() -> ProviderChannel {
        ProviderChannel {
            channel_id: "fake".into(),
            provider: "fake".into(),
            wire_format: WireFormat::Messages,
            base_url: None,
            api_key: String::new(),
            model: "fake-model".into(),
            stream: true,
            enabled: true,
            supports_vision: false,
            supports_thinking: false,
            max_output_tokens: None,
            context_window_tokens: None,
            thinking_budget_tokens: None,
        }
    }

    /// Build a scripted outcome the way a real provider does: run SkillCallParser over the
    /// scripted raw text, populate `skill_events` (TextDelta excluded). TextDelta would be
    /// emitted by the provider during streaming; here we simply build the outcome.
    fn scripted_outcome(
        text: &str,
        usage_input: u32,
        usage_output: u32,
        stop_reason: AgentStopReason,
    ) -> ProviderTurnOutcome {
        let mut parser = SkillCallParser::new();
        let mut events = parser.feed(text);
        events.extend(parser.finalize());
        let skill_events: Vec<ParserEvent> = events
            .into_iter()
            .filter(|e| !matches!(e, ParserEvent::TextDelta(_)))
            .collect();
        ProviderTurnOutcome {
            text: text.to_string(),
            usage_input,
            usage_output,
            stop_reason,
            skill_events,
        }
    }

    /// Fake provider — 按预设脚本输出 turns.
    /// FIX 1: emit clean TextDelta from the SkillCallParser over the scripted text (mirrors
    /// HttpProvider), and carry parsed skill_events in the outcome.
    struct ScriptedProvider {
        script: Vec<Result<ProviderTurnOutcome, LoopError>>,
        index: usize,
    }
    #[async_trait::async_trait]
    impl ProviderStream for ScriptedProvider {
        async fn next_turn(
            &mut self,
            _messages: &[AgentMessage],
            _context: &ContextBundle,
            event_tx: &Sender<AgentEvent>,
            run_id: &str,
        ) -> Result<ProviderTurnOutcome, LoopError> {
            // Move element out without cloning LoopError (LoopError: !Clone).
            if self.index >= self.script.len() {
                return Err(LoopError::Provider("scripted ran out".into()));
            }
            let item = std::mem::replace(
                &mut self.script[self.index],
                Err(LoopError::Provider("consumed".into())),
            );
            self.index += 1;
            // Emit clean TextDelta in real time, as a streaming provider would.
            if let Ok(out) = &item {
                let mut parser = SkillCallParser::new();
                let mut events = parser.feed(&out.text);
                events.extend(parser.finalize());
                for ev in events {
                    if let ParserEvent::TextDelta(s) = ev {
                        send_event(
                            event_tx,
                            AgentEvent::TextDelta {
                                run_id: run_id.to_string(),
                                delta: s,
                            },
                        )
                        .await?;
                    }
                }
            }
            item
        }
    }

    fn echo_handler() -> Arc<dyn SkillHandler> {
        Arc::new(FnSkillHandler(|inv: SkillInvocation| {
            Box::pin(async move {
                SkillHandlerOutput::ok(json!({"echoed": inv.input}))
            }) as SkillHandlerFuture
        }))
    }

    fn spec(name: &str) -> SkillSpec {
        SkillSpec::new(
            name,
            "test",
            json!({"type":"object"}),
            vec![format!(r#"<use_skill name="{}">{{}}</use_skill>"#, name)],
            5000,
            SideEffect::None,
        )
    }

    fn req() -> AgentRunRequest {
        AgentRunRequest {
            run_id: "r1".into(),
            trigger: "u".into(),
            channel: channel(),
            max_turns: 5,
            seed_messages: vec![],
        }
    }

    #[tokio::test]
    async fn loop_completes_on_text_only_turn() {
        let registry = Arc::new(SkillRegistry::new_without_persist());
        let provider = Box::new(ScriptedProvider {
            script: vec![Ok(scripted_outcome(
                "hello",
                5,
                7,
                AgentStopReason::Completed,
            ))],
            index: 0,
        });
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        let summary =
            run_agent_loop(req(), registry, ContextBundle::new("r1"), provider, tx)
                .await
                .unwrap();
        assert_eq!(summary.turns, 1);
        assert_eq!(summary.stop_reason, AgentStopReason::Completed);
        assert_eq!(summary.input_tokens, 5);
        assert_eq!(summary.output_tokens, 7);

        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        assert!(matches!(events.first(), Some(AgentEvent::RunStart { .. })));
        assert!(matches!(events.last(), Some(AgentEvent::Done { .. })));
    }

    #[tokio::test]
    async fn loop_dispatches_single_skill_then_completes() {
        let registry = Arc::new(SkillRegistry::new_without_persist());
        registry.register_skill(spec("echo"), echo_handler()).unwrap();
        let provider = Box::new(ScriptedProvider {
            script: vec![
                Ok(scripted_outcome(
                    r#"check: <use_skill name="echo">{"a":1}</use_skill>"#,
                    1,
                    1,
                    AgentStopReason::ProviderStop,
                )),
                Ok(scripted_outcome("done", 1, 1, AgentStopReason::Completed)),
            ],
            index: 0,
        });
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
        let summary =
            run_agent_loop(req(), registry, ContextBundle::new("r1"), provider, tx)
                .await
                .unwrap();
        assert_eq!(summary.stop_reason, AgentStopReason::Completed);
        assert_eq!(summary.turns, 2);
        assert_eq!(summary.skill_call_ids.len(), 1);

        // FIX 1: a `<use_skill>` turn emits clean TextDelta (raw XML suppressed) exactly
        // once, and the skill still dispatches.
        let mut text_deltas: Vec<String> = Vec::new();
        let mut starts2 = 0;
        let mut rx_events = Vec::new();
        while let Some(e) = rx.recv().await {
            rx_events.push(e);
        }
        for e in &rx_events {
            match e {
                AgentEvent::TextDelta { delta, .. } => text_deltas.push(delta.clone()),
                AgentEvent::SkillStart { .. } => starts2 += 1,
                _ => {}
            }
        }
        let joined: String = text_deltas.concat();
        assert!(
            !joined.contains("<use_skill"),
            "raw <use_skill> XML leaked into TextDelta: {joined:?}"
        );
        assert!(joined.contains("check: "));
        // "check: " appears exactly once (no double-emit).
        assert_eq!(joined.matches("check: ").count(), 1, "TextDelta double-emitted");
        assert_eq!(starts2, 1);

        let ends = rx_events
            .iter()
            .filter(|e| matches!(e, AgentEvent::SkillEnd { .. }))
            .count();
        assert_eq!(starts2, 1);
        assert_eq!(ends, 1);
    }

    #[tokio::test]
    async fn loop_handles_multiple_skills_in_one_turn_in_order() {
        let registry = Arc::new(SkillRegistry::new_without_persist());
        registry.register_skill(spec("a"), echo_handler()).unwrap();
        registry.register_skill(spec("b"), echo_handler()).unwrap();
        let provider = Box::new(ScriptedProvider {
            script: vec![
                Ok(scripted_outcome(
                    r#"<use_skill name="a">{"i":1}</use_skill><use_skill name="b">{"i":2}</use_skill>"#,
                    1,
                    1,
                    AgentStopReason::ProviderStop,
                )),
                Ok(scripted_outcome("done", 0, 0, AgentStopReason::Completed)),
            ],
            index: 0,
        });
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
        let summary =
            run_agent_loop(req(), registry, ContextBundle::new("r1"), provider, tx)
                .await
                .unwrap();
        assert_eq!(summary.skill_call_ids.len(), 2);
        let mut ordered_names: Vec<String> = Vec::new();
        while let Some(e) = rx.recv().await {
            if let AgentEvent::SkillStart { name, .. } = e {
                ordered_names.push(name);
            }
        }
        assert_eq!(ordered_names, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn loop_reactive_retry_recovers_on_first_failure() {
        let registry = Arc::new(SkillRegistry::new_without_persist());
        let provider = Box::new(ScriptedProvider {
            script: vec![
                Err(LoopError::ProviderContextTooLong),
                Ok(scripted_outcome("ok", 1, 1, AgentStopReason::Completed)),
            ],
            index: 0,
        });
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        let summary =
            run_agent_loop(req(), registry, ContextBundle::new("r1"), provider, tx)
                .await
                .unwrap();
        assert_eq!(summary.stop_reason, AgentStopReason::Completed);
        let mut saw_compacted = false;
        while let Some(e) = rx.recv().await {
            if let AgentEvent::Compacted { tier, .. } = e {
                if matches!(tier, crate::domain::agent::CompactedTier::ReactiveRetry) {
                    saw_compacted = true;
                }
            }
        }
        assert!(saw_compacted);
    }

    #[tokio::test]
    async fn loop_reactive_retry_fails_closed_on_second_context_too_long() {
        let registry = Arc::new(SkillRegistry::new_without_persist());
        let provider = Box::new(ScriptedProvider {
            script: vec![
                Err(LoopError::ProviderContextTooLong),
                Err(LoopError::ProviderContextTooLong),
            ],
            index: 0,
        });
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        let summary =
            run_agent_loop(req(), registry, ContextBundle::new("r1"), provider, tx)
                .await
                .unwrap();
        assert_eq!(summary.stop_reason, AgentStopReason::ContextLimit);
        let mut saw_error_with_code = false;
        while let Some(e) = rx.recv().await {
            if let AgentEvent::Error { code, .. } = e {
                if matches!(code, ErrorCode::ProviderContextTooLong) {
                    saw_error_with_code = true;
                }
            }
        }
        assert!(saw_error_with_code);
    }

    #[tokio::test]
    async fn loop_prepends_skill_list_to_system_parts_on_start() {
        // Spec §2 / §5 line 533: 每次 Agent loop 启动时,Infra 自动 prepend SkillSpec 清单
        // 到 ContextBundle.systemParts;Runtime 不需要手动塞。
        let registry = Arc::new(SkillRegistry::new_without_persist());
        registry.register_skill(spec("echo"), echo_handler()).unwrap();

        // Capture the systemParts via a custom provider that snapshots context.
        struct SnapshotProvider {
            captured: Arc<std::sync::Mutex<Vec<crate::domain::agent::ContextPart>>>,
        }
        #[async_trait::async_trait]
        impl ProviderStream for SnapshotProvider {
            async fn next_turn(
                &mut self,
                _messages: &[AgentMessage],
                context: &ContextBundle,
                _event_tx: &Sender<AgentEvent>,
                _run_id: &str,
            ) -> Result<ProviderTurnOutcome, LoopError> {
                *self.captured.lock().unwrap() = context.system_parts.clone();
                Ok(ProviderTurnOutcome {
                    text: "bye".into(),
                    usage_input: 1,
                    usage_output: 1,
                    stop_reason: AgentStopReason::Completed,
                    skill_events: Vec::new(),
                })
            }
        }
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Box::new(SnapshotProvider {
            captured: Arc::clone(&captured),
        });
        let (tx, _rx) = mpsc::channel::<AgentEvent>(64);
        let _ = run_agent_loop(req(), registry, ContextBundle::new("r1"), provider, tx)
            .await
            .unwrap();
        let parts = captured.lock().unwrap().clone();
        assert!(!parts.is_empty(), "system_parts should have been prepended");
        let head = match &parts[0].content {
            crate::domain::agent::ContextContent::Text(s) => s.clone(),
            _ => panic!("expected Text"),
        };
        assert!(head.contains("## echo"), "system prompt missing skill section: {}", head);
        assert!(head.contains("use_skill"), "missing protocol preamble: {}", head);
        // droppable=false invariant (skill list is identity / system content)
        assert!(!parts[0].droppable);
    }

    #[tokio::test]
    async fn loop_emits_run_start_with_model_from_channel() {
        let registry = Arc::new(SkillRegistry::new_without_persist());
        let provider = Box::new(ScriptedProvider {
            script: vec![Ok(scripted_outcome("x", 0, 0, AgentStopReason::Completed))],
            index: 0,
        });
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        let _ = run_agent_loop(req(), registry, ContextBundle::new("r1"), provider, tx)
            .await
            .unwrap();
        // First event should be RunStart with the channel's model.
        let first = rx.recv().await.unwrap();
        match first {
            AgentEvent::RunStart { model, .. } => assert_eq!(model, "fake-model"),
            other => panic!("expected RunStart, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn loop_reports_unknown_skill_as_error_to_model() {
        // 模型尝试调用未注册 skill → loop 不 panic；下一轮回写 <skill_error code="invalid_input">.
        let registry = Arc::new(SkillRegistry::new_without_persist());
        let provider = Box::new(ScriptedProvider {
            script: vec![
                Ok(scripted_outcome(
                    r#"<use_skill name="missing">{}</use_skill>"#,
                    1,
                    1,
                    AgentStopReason::ProviderStop,
                )),
                Ok(scripted_outcome("bye", 1, 1, AgentStopReason::Completed)),
            ],
            index: 0,
        });
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        let summary =
            run_agent_loop(req(), registry, ContextBundle::new("r1"), provider, tx)
                .await
                .unwrap();
        assert_eq!(summary.stop_reason, AgentStopReason::Completed);
        let mut saw_error_end = false;
        while let Some(e) = rx.recv().await {
            if let AgentEvent::SkillEnd { is_error, .. } = e {
                if is_error {
                    saw_error_end = true;
                }
            }
        }
        assert!(saw_error_end);
    }

    /// 端到端实网 chat：跑完整 `run_agent_loop`（loop + HttpProvider + 真 SSE）对三个真实
    /// relay，用真实问题，打印流式答案。`#[ignore]`，凭证全走 env（无硬编码 secret）。
    ///   TEST_OAI_BASE/KEY/MODEL（responses）, TEST_ANT_BASE/KEY/MODEL（messages）,
    ///   TEST_DS_BASE/KEY/MODEL（chat_completions） → cargo test loop_chat_live -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn loop_chat_live() {
        use crate::infrastructure::agent::http_provider::HttpProvider;
        use crate::infrastructure::agent::skill_registry::SkillRegistry;
        use chrono::Utc;

        async fn one(label: &str, wire: WireFormat, base: String, key: String, model: String) {
            let mut ch = channel();
            ch.wire_format = wire;
            ch.base_url = Some(base);
            ch.api_key = key;
            ch.model = model;
            ch.max_output_tokens = Some(512);
            let provider = Box::new(HttpProvider::new(ch.clone()).unwrap());
            let request = AgentRunRequest {
                run_id: "live".into(),
                trigger: "user".into(),
                channel: ch,
                max_turns: 1,
                seed_messages: vec![AgentMessage {
                    message_id: "m1".into(),
                    run_id: Some("live".into()),
                    role: AgentMessageRole::User,
                    blocks: vec![AgentMessageBlock::Text {
                        text: "用一句话解释A股的T+1交易制度".into(),
                    }],
                    created_at: Utc::now(),
                }],
            };
            let registry = Arc::new(SkillRegistry::new_without_persist());
            let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
            let pump = tokio::spawn(async move {
                let mut text = String::new();
                while let Some(e) = rx.recv().await {
                    if let AgentEvent::TextDelta { delta, .. } = e {
                        text.push_str(&delta);
                    }
                }
                text
            });
            let summary =
                run_agent_loop(request, registry, ContextBundle::new("live"), provider, tx)
                    .await
                    .unwrap_or_else(|e| panic!("[{label}] loop failed: {e}"));
            let streamed = pump.await.unwrap();
            println!("[loop-live][{label}] stop={:?} answer={:?}", summary.stop_reason, streamed);
            assert!(!streamed.is_empty(), "[{label}] empty answer");
        }

        let mut ran = 0;
        if let (Ok(b), Ok(k)) = (std::env::var("TEST_OAI_BASE"), std::env::var("TEST_OAI_KEY")) {
            let m = std::env::var("TEST_OAI_MODEL").unwrap_or_else(|_| "gpt-5".into());
            one("responses", WireFormat::Responses, b, k, m).await;
            ran += 1;
        }
        if let (Ok(b), Ok(k)) = (std::env::var("TEST_ANT_BASE"), std::env::var("TEST_ANT_KEY")) {
            let m = std::env::var("TEST_ANT_MODEL")
                .unwrap_or_else(|_| "claude-haiku-4-5-20251001".into());
            one("messages", WireFormat::Messages, b, k, m).await;
            ran += 1;
        }
        if let Ok(k) = std::env::var("TEST_DS_KEY") {
            let b = std::env::var("TEST_DS_BASE").unwrap_or_else(|_| "https://api.deepseek.com".into());
            let m = std::env::var("TEST_DS_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".into());
            one("chat_completions", WireFormat::ChatCompletions, b, k, m).await;
            ran += 1;
        }
        println!("[loop-live] ran {ran} end-to-end loop checks");
        assert!(ran > 0, "no TEST_* env provided");
    }
}
