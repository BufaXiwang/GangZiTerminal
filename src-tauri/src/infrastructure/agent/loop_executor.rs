//! Canonical Agent loop 编排（infra 内部 use case）。
//!
//! Spec: docs/design/agent-infra-module.md §3 Agent Loop，§4 Reactive retry，§5 Infra Loop API
//!
//! 行为：
//! 1. emit `run_start`
//! 2. 调 provider stream（trait `ProviderStream`，便于注入 fake provider 测试）
//! 3. （spec §2/§3）：`SkillCallParser` 现在跑在 **provider 内部**，clean `TextDelta`
//!    由 provider 实时 emit（XML 抑制）。loop 只消费 `outcome.skill_events`：
//!    - `UseSkill` → emit `skill_start` → `SkillRegistry::dispatch_skill_call` →
//!      emit `skill_end` → 缓存 `<skill_result>` 文本
//!    - `ParseError` → 把 `<skill_error code="parse_error">` 加到本轮回写文本
//!
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
    AgentStopReason, CompactedTier, CompactionConfig, ContextBundle, ContextContent, ContextPart,
    ContextPartKind, MessageKind, ProviderChannel, RunSummary, SideEffect,
};
use crate::domain::shared::ErrorCode;
use crate::infrastructure::agent::context_compaction::{
    compact_context, drop_oldest_round_messages, estimate_context_tokens, micro_clear_messages,
    CompactPolicy,
};
use crate::infrastructure::agent::http_provider::HttpProvider;
use crate::infrastructure::agent::messages_repo::AgentMessagesRepo;
use crate::infrastructure::agent::skill_parser::ParserEvent;
use crate::infrastructure::agent::skill_registry::{DispatchError, SkillRegistry};
use crate::infrastructure::agent::system_prompt::build_system_prompt;
use chrono::Utc;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc::{self, Sender};
use uuid::Uuid;

/// 默认尾窗：最近 N 轮永不摘要 / micro-clear（spec §4 keepRecentTurns 缺省）。
const DEFAULT_KEEP_RECENT_TURNS: u32 = 3;

/// 从 `CompactionConfig`（可空）+ channel 推导出三档阈值 + 尾窗。
///
/// Spec §4 / §5：阈值优先取 `CompactionConfig`；缺省由 `channel.contextWindowTokens` 推导：
/// soft = window/2、summarize = window*0.7、hard = window*0.9。channel 也没声明窗口时退化为
/// `ContextWindowLimits::default()`（60k / 90k / 180k）。
struct CompactionPlan {
    soft_limit: u32,
    summarize_threshold: u32,
    hard_limit: u32,
    keep_recent_turns: u32,
    summarize_prompt: Option<String>,
    compact_channel: Option<ProviderChannel>,
}

impl CompactionPlan {
    fn derive(cfg: Option<&CompactionConfig>, channel: &ProviderChannel) -> Self {
        let defaults = crate::domain::agent::ContextWindowLimits::default();
        let (win_soft, win_summarize, win_hard) = match channel.context_window_tokens {
            Some(w) => {
                let w = w as u64;
                (
                    (w / 2) as u32,
                    (w * 7 / 10) as u32,
                    (w * 9 / 10) as u32,
                )
            }
            None => (
                defaults.soft_limit_tokens,
                defaults.summarize_threshold_tokens,
                defaults.hard_limit_tokens,
            ),
        };
        CompactionPlan {
            soft_limit: cfg.and_then(|c| c.soft_limit_tokens).unwrap_or(win_soft),
            summarize_threshold: cfg
                .and_then(|c| c.summarize_threshold_tokens)
                .unwrap_or(win_summarize),
            hard_limit: cfg.and_then(|c| c.hard_limit_tokens).unwrap_or(win_hard),
            keep_recent_turns: cfg
                .and_then(|c| c.keep_recent_turns)
                .unwrap_or(DEFAULT_KEEP_RECENT_TURNS),
            summarize_prompt: cfg.and_then(|c| c.summarize_prompt.clone()),
            compact_channel: cfg.and_then(|c| c.compact_channel.clone()),
        }
    }
}

/// 一次 provider stream 的拉取结果。
///
/// 本 trait 抽象掉具体 SSE 解码细节，让 loop 测试可以注入 mock。
///
/// （spec §2/§3）：`SkillCallParser` 现在跑在 **streaming provider 内部**，
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
    /// 致命 provider 错误（4xx 非 429 / 鉴权 / 请求格式 / wire 映射）：不重试、不 fallback。
    #[error("provider error: {0}")]
    Provider(String),
    /// Spec §4: 瞬时 provider 错误（5xx / 429 / 超时 / 连接 / 上游失败）：可退避重试 + 渠道 fallback。
    #[error("provider transient error: {0}")]
    ProviderTransient(String),
    /// Spec §4: provider 返回 context-too-long（HTTP 400 或等价 error code）。
    #[error("provider context too long")]
    ProviderContextTooLong,
    #[error("event channel closed")]
    EventChannelClosed,
}

/// 瞬时错误退避重试计划（从 `RetryConfig` 推导；缺省 3 次 / 500ms / 8000ms）。spec §4。
#[derive(Debug, Clone, Copy)]
struct RetryPlan {
    max_attempts_per_channel: u32,
    base_backoff_ms: u64,
    max_backoff_ms: u64,
}

impl RetryPlan {
    fn derive(cfg: Option<&crate::domain::agent::RetryConfig>) -> Self {
        Self {
            max_attempts_per_channel: cfg
                .and_then(|c| c.max_attempts_per_channel)
                .unwrap_or(3)
                .max(1),
            base_backoff_ms: cfg.and_then(|c| c.base_backoff_ms).unwrap_or(500),
            // Guard against an inverted config (max < base) collapsing backoff to a constant.
            max_backoff_ms: cfg
                .and_then(|c| c.max_backoff_ms)
                .unwrap_or(8000)
                .max(cfg.and_then(|c| c.base_backoff_ms).unwrap_or(500)),
        }
    }

    /// 第 `attempt`（1-based）次失败后、下一次重试前的退避毫秒数：`base * 2^(attempt-1)`，封顶。
    fn backoff_ms(&self, attempt: u32) -> u64 {
        let shifted = self
            .base_backoff_ms
            .saturating_mul(1u64.checked_shl(attempt.saturating_sub(1).min(20)).unwrap_or(u64::MAX));
        shifted.min(self.max_backoff_ms)
    }
}

/// Spec §4 容错机制：在有序 `providers` 上调 `next_turn`，对**瞬时**错误同渠道指数退避重试，
/// 耗尽后切到下一渠道（sticky：`active_idx` 推进，后续 turn 不再回头试已死渠道）。
/// `ProviderContextTooLong` 与致命错误**原样向上抛**（由 loop 的 reactive-retry / fail-closed 处理）。
async fn resilient_next_turn(
    providers: &mut [Box<dyn ProviderStream>],
    active_idx: &mut usize,
    plan: &RetryPlan,
    messages: &[AgentMessage],
    context: &ContextBundle,
    event_tx: &Sender<AgentEvent>,
    run_id: &str,
) -> Result<ProviderTurnOutcome, LoopError> {
    let n = providers.len();
    loop {
        let ci = *active_idx;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match providers[ci]
                .next_turn(messages, context, event_tx, run_id)
                .await
            {
                Ok(out) => return Ok(out),
                Err(LoopError::ProviderContextTooLong) => {
                    return Err(LoopError::ProviderContextTooLong)
                }
                Err(LoopError::ProviderTransient(msg)) => {
                    if attempt < plan.max_attempts_per_channel {
                        let backoff = plan.backoff_ms(attempt);
                        tracing::warn!(
                            run_id,
                            channel_idx = ci,
                            attempt,
                            backoff_ms = backoff,
                            "provider transient error, retrying same channel: {msg}"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                        continue;
                    }
                    // attempts exhausted on this channel → fall back to the next one, if any.
                    if ci + 1 < n {
                        tracing::warn!(
                            run_id,
                            from_channel = ci,
                            to_channel = ci + 1,
                            "provider channel exhausted, falling back: {msg}"
                        );
                        *active_idx = ci + 1;
                        break; // re-enter outer loop with the next channel
                    }
                    return Err(LoopError::ProviderTransient(msg)); // all channels exhausted
                }
                Err(e) => return Err(e), // fatal: no retry, no fallback
            }
        }
    }
}

/// 唯一 Agent loop 入口：推进一个会话 turn（一次 user→assistant，内部可多次 provider / skill 往返）。
///
/// **持久化与续接全归 Infra**（spec §4）。调用方只给 `request.input`（这一轮的新消息，通常一条 user）
/// + 可选 `conversation_id` + 可选 `repo`：
/// - `repo=Some` 且有 `conversation_id` → Infra 先把 `input` 落库（分配 seq + 打 conversationId），
///   再 `load_conversation_view`（含刚落库的 input）作为本轮上下文，跑完把产出也落库。
/// - 无 `conversation_id` 或 `repo=None` → 不 load 不持久化，`input` 即本轮全部上下文（无状态运行）。
///
/// 调用方**永不**自己 upsert / 分配 seq / load 历史。主动压缩 + Summarize 也都在此函数内完成。
///
/// Spec §3 Agent Loop，§4 上下文管理（主动压缩 + Summarize + 多轮持久化），§5 Infra Loop API。
pub async fn run_agent_turn(
    mut request: AgentRunRequest,
    registry: Arc<SkillRegistry>,
    mut context: ContextBundle,
    mut providers: Vec<Box<dyn ProviderStream>>,
    event_tx: Sender<AgentEvent>,
    repo: Option<AgentMessagesRepo>,
) -> Result<RunSummary, LoopError> {
    let run_id = request.run_id.clone();
    let plan = CompactionPlan::derive(request.compaction.as_ref(), &request.channel);
    let retry_plan = RetryPlan::derive(request.retry.as_ref());
    let mut active_provider_idx: usize = 0; // §4 fallback: sticky index into `providers`
    let conversation_id = request.conversation_id.clone();
    if providers.is_empty() {
        return Err(LoopError::Provider(
            "run_agent_turn called with no providers (need ≥1: the primary channel)".into(),
        ));
    }

    // ---- 消息持久化 + 续接全归 Infra（spec §4）----
    // 有 conversationId + repo：先把这一轮 input 落库（分配 seq + 打 conversationId + 默认 kind=Chat），
    // 再 load 压缩视图作为本轮上下文（含刚落库的 input）；否则（无状态 / 新会话）直接用 input。
    let mut messages: Vec<AgentMessage> = match (&conversation_id, &repo) {
        (Some(conv), Some(r)) => {
            for m in request.input.iter_mut() {
                m.conversation_id = Some(conv.clone());
                m.seq = r.next_seq(conv).ok();
                if m.kind.is_none() {
                    m.kind = Some(MessageKind::Chat);
                }
                persist_message(Some(r), m, &event_tx, &run_id).await;
            }
            r.load_conversation_view(conv)
                .map_err(|e| LoopError::Provider(format!("load_conversation_view: {e}")))?
        }
        _ => request.input.clone(),
    };

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

    let mut skill_call_ids: Vec<String> = Vec::new();
    let mut usage_input: u32 = 0;
    let mut usage_output: u32 = 0;
    // Spec §4 generic durable signal: message_ids that must never be compacted/stubbed.
    // The loop derives this set ONLY from SkillSpec.sideEffect == trading_write (Infra knows
    // nothing about "trading" semantics — it just honours the generic side-effect flag) and from
    // kind=summary checkpoints. Seed summary messages are durable too.
    let mut durable_message_ids: HashSet<String> = messages
        .iter()
        .filter(|m| m.kind == Some(MessageKind::Summary))
        .map(|m| m.message_id.clone())
        .collect();

    let mut turn: u32 = 0;
    let mut reactive_retry_used = false;
    let stop_reason: AgentStopReason;
    'outer: loop {
        if turn >= request.max_turns {
            stop_reason = AgentStopReason::MaxTurns;
            break;
        }
        turn += 1;

        // ---- Spec §4: proactive per-turn compaction BEFORE provider.next_turn ----
        // estimate(messages + context) → if > soft_limit: MicroClear; if still > summarize
        // threshold: Summarize (when summarize_prompt present) else Drop-oldest. Generic signals
        // only: ContextPart.droppable + durable_message_ids.
        let new_durable = proactive_compact(
            &mut messages,
            &mut context,
            &durable_message_ids,
            &plan,
            &request.channel,
            repo.as_ref(),
            &event_tx,
            &run_id,
        )
        .await?;
        durable_message_ids.extend(new_durable);

        // ---- provider call: transient backoff retry + channel fallback (§4), then the
        // orthogonal context-too-long reactive-retry path below. ----
        let outcome = match resilient_next_turn(
            &mut providers,
            &mut active_provider_idx,
            &retry_plan,
            &messages,
            &context,
            &event_tx,
            &run_id,
        )
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

        // ---- provider already ran SkillCallParser and emitted clean TextDelta.
        // Loop consumes only the parsed skill events; it does NOT re-feed text and does
        // NOT re-emit TextDelta. Assistant message history uses outcome.text (raw XML).
        let mut skill_results_for_next_turn: Vec<String> = Vec::new();
        let mut any_dispatch = false;
        // Spec §4: a turn whose skill_result message carries any trading_write skill result is
        // durable (never compacted). Generic signal — Infra only reads SkillSpec.sideEffect.
        let mut turn_has_trading_write = false;

        for ev in outcome.skill_events.iter().cloned() {
            match ev {
                ParserEvent::TextDelta(_) => {
                    // Provider already emitted clean TextDelta; loop ignores any here.
                }
                ParserEvent::UseSkill { name, input } => {
                    any_dispatch = true;
                    if registry.skill_side_effect(&name) == Some(SideEffect::TradingWrite) {
                        turn_has_trading_write = true;
                    }
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
            let msg = build_message(
                AgentMessageRole::Assistant,
                outcome.text.clone(),
                &run_id,
                &conversation_id,
                repo.as_ref(),
            );
            persist_message(repo.as_ref(), &msg, &event_tx, &run_id).await;
            messages.push(msg);
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
        let user_msg = build_message(
            AgentMessageRole::User,
            user_text,
            &run_id,
            &conversation_id,
            repo.as_ref(),
        );
        // Spec §4: trading_write skill_result message is durable (never compacted/stubbed).
        if turn_has_trading_write {
            durable_message_ids.insert(user_msg.message_id.clone());
        }
        persist_message(repo.as_ref(), &user_msg, &event_tx, &run_id).await;
        messages.push(user_msg);
        // Continue to next turn.
    }

    // usage event semantics.
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

/// 构造一条会话消息：填 conversation_id + 单调 seq（有 repo + conversation_id 时取 `next_seq`）。
fn build_message(
    role: AgentMessageRole,
    text: String,
    run_id: &str,
    conversation_id: &Option<String>,
    repo: Option<&AgentMessagesRepo>,
) -> AgentMessage {
    let seq = match (conversation_id, repo) {
        (Some(conv), Some(r)) => r.next_seq(conv).ok(),
        _ => None,
    };
    AgentMessage {
        message_id: format!("am-{}", Uuid::new_v4()),
        run_id: Some(run_id.to_string()),
        conversation_id: conversation_id.clone(),
        seq,
        kind: Some(MessageKind::Chat),
        role,
        blocks: vec![AgentMessageBlock::Text { text }],
        created_at: Utc::now(),
    }
}

/// 持久化一条消息（全量审计真源；spec §4）。无 repo 或无 conversation_id → 跳过（测试场景）。
/// 持久化失败不终止 loop：只 emit 一个非致命 error event（loop 仍可继续）。
async fn persist_message(
    repo: Option<&AgentMessagesRepo>,
    msg: &AgentMessage,
    event_tx: &Sender<AgentEvent>,
    run_id: &str,
) {
    if msg.conversation_id.is_none() {
        return;
    }
    if let Some(r) = repo {
        if let Err(e) = r.upsert_message(msg) {
            let _ = event_tx
                .send(AgentEvent::Error {
                    run_id: run_id.to_string(),
                    code: ErrorCode::DbError,
                    message: format!("persist message failed: {e}"),
                })
                .await;
        }
    }
}

/// Spec §4 主动压缩（每轮发请求前）：estimate(messages + context) →
/// - `> soft_limit`：MicroClear（context 易腐 part 替 stub + messages 非 durable 旧 skill_result 替 stub）
/// - 仍 `> summarize_threshold`：有 `summarize_prompt` → Summarize（模型调用）；否则 Drop 最旧一轮
///
/// 只认通用信号：`ContextPart.droppable` + `durable_message_ids`。trading_write / summary 永不动。
/// 返回本次新增的 durable message_ids（如 Summarize 产出的 summary 检查点），caller 并入集合。
#[allow(clippy::too_many_arguments)]
async fn proactive_compact(
    messages: &mut Vec<AgentMessage>,
    context: &mut ContextBundle,
    durable_message_ids: &HashSet<String>,
    plan: &CompactionPlan,
    channel: &ProviderChannel,
    repo: Option<&AgentMessagesRepo>,
    event_tx: &Sender<AgentEvent>,
    run_id: &str,
) -> Result<HashSet<String>, LoopError> {
    let mut new_durable: HashSet<String> = HashSet::new();
    let est = estimate_context_tokens(messages, context, channel);
    if est.total_tokens <= plan.soft_limit {
        return Ok(new_durable);
    }

    // ---- Tier 1: MicroClear ----
    let before = est.total_tokens;
    let (new_ctx, ctx_stubbed) =
        compact_context(context.clone(), CompactPolicy::MicroClear, plan_limits(plan));
    *context = new_ctx;
    let msg_stubbed = micro_clear_messages(
        messages,
        durable_message_ids,
        plan.keep_recent_turns as usize,
    );
    let after_micro = estimate_context_tokens(messages, context, channel).total_tokens;
    if ctx_stubbed + msg_stubbed > 0 {
        send_event(
            event_tx,
            AgentEvent::Compacted {
                run_id: run_id.to_string(),
                tier: CompactedTier::MicroClear,
                dropped_messages: ctx_stubbed + msg_stubbed,
                estimated_tokens_saved: Some(before.saturating_sub(after_micro)),
            },
        )
        .await?;
    }

    if after_micro <= plan.summarize_threshold {
        return Ok(new_durable);
    }

    // ---- Tier 2: Summarize (if prompt provided) else Drop oldest round ----
    if let Some(prompt) = plan.summarize_prompt.clone() {
        match run_summarize(
            messages,
            durable_message_ids,
            &prompt,
            plan.compact_channel.clone().unwrap_or_else(|| channel.clone()),
            plan.keep_recent_turns as usize,
            run_id,
        )
        .await
        {
            Ok(Some((summary_msg, replaced))) => {
                let after = estimate_context_tokens(messages, context, channel).total_tokens;
                send_event(
                    event_tx,
                    AgentEvent::Compacted {
                        run_id: run_id.to_string(),
                        tier: CompactedTier::Summarize,
                        dropped_messages: replaced,
                        estimated_tokens_saved: Some(after_micro.saturating_sub(after)),
                    },
                )
                .await?;
                // Summary checkpoint is durable + persisted to agent_messages (spec §4).
                new_durable.insert(summary_msg.message_id.clone());
                persist_message(repo, &summary_msg, event_tx, run_id).await;
            }
            Ok(None) => { /* nothing to summarize */ }
            Err(_) => {
                // Summarize model call failed → fall back to Drop oldest round (guarantees shorter).
                let dropped = drop_oldest_round_messages(
                    messages,
                    durable_message_ids,
                    plan.keep_recent_turns as usize,
                );
                if dropped > 0 {
                    let after = estimate_context_tokens(messages, context, channel).total_tokens;
                    send_event(
                        event_tx,
                        AgentEvent::Compacted {
                            run_id: run_id.to_string(),
                            tier: CompactedTier::Drop,
                            dropped_messages: dropped,
                            estimated_tokens_saved: Some(after_micro.saturating_sub(after)),
                        },
                    )
                    .await?;
                }
            }
        }
    } else {
        // Spec §4: no summarize_prompt → Summarize tier degrades to Drop (keeps durable inline).
        let dropped = drop_oldest_round_messages(
            messages,
            durable_message_ids,
            plan.keep_recent_turns as usize,
        );
        if dropped > 0 {
            let after = estimate_context_tokens(messages, context, channel).total_tokens;
            send_event(
                event_tx,
                AgentEvent::Compacted {
                    run_id: run_id.to_string(),
                    tier: CompactedTier::Drop,
                    dropped_messages: dropped,
                    estimated_tokens_saved: Some(after_micro.saturating_sub(after)),
                },
            )
            .await?;
        }
    }
    Ok(new_durable)
}

fn plan_limits(plan: &CompactionPlan) -> crate::domain::agent::ContextWindowLimits {
    crate::domain::agent::ContextWindowLimits {
        soft_limit_tokens: plan.soft_limit,
        summarize_threshold_tokens: plan.summarize_threshold,
        hard_limit_tokens: plan.hard_limit,
        micro_clear_after_secs: crate::domain::agent::ContextWindowLimits::default()
            .micro_clear_after_secs,
    }
}

/// Spec §4 Summarize 执行（模型调用归 loop，不在 `compact_context`）：
/// 对尾窗外（最近 `keep_recent` 之前）的非 durable 消息做一次性 collected 摘要调用
/// （system = summarize_prompt，user = 待压消息序列化），把它们替换为单条 `kind=Summary`
/// durable 消息。返回 `Some((summary_msg, replaced_count))`；无可压消息 → `None`。
///
/// 复用 `HttpProvider` + 一个丢弃事件的临时 mpsc sender——**不**把摘要 delta 泄漏到 run 的 event_tx。
async fn run_summarize(
    messages: &mut Vec<AgentMessage>,
    durable_message_ids: &HashSet<String>,
    summarize_prompt: &str,
    compact_channel: ProviderChannel,
    keep_recent: usize,
    run_id: &str,
) -> Result<Option<(AgentMessage, u32)>, LoopError> {
    let cutoff = messages.len().saturating_sub(keep_recent);
    // Collect indices to summarize (tail-window-excluded prefix).
    // ROLLING SUMMARY: a prior `kind=Summary` checkpoint IS folded into the input and
    // replaced by the new (cumulative) summary — so multi-cycle compaction never loses the
    // earliest history (load_conversation_view returns only the latest summary). Only
    // trading_write / durable results are kept inline verbatim (never summarized — audit /
    // double-trade safety). Skip if there is no NEW (non-summary) content to fold, to avoid
    // pointlessly re-summarizing a lone prior summary.
    let mut to_summarize_idx: Vec<usize> = Vec::new();
    let mut has_new_content = false;
    for (i, m) in messages.iter().enumerate() {
        if i >= cutoff {
            break;
        }
        let is_summary = m.kind == Some(MessageKind::Summary);
        // Prior summaries are ALWAYS folded (even though they're durable for MicroClear/Drop),
        // so multi-cycle compaction accumulates rather than loses history. Other durables
        // (trading_write etc.) are kept inline verbatim — never summarized.
        if !is_summary && durable_message_ids.contains(&m.message_id) {
            continue;
        }
        if !is_summary {
            has_new_content = true;
        }
        to_summarize_idx.push(i);
    }
    if to_summarize_idx.is_empty() || !has_new_content {
        return Ok(None);
    }

    // Serialize the messages-to-summarize. ROLLING: prior summary checkpoints are
    // demarcated under an explicit "existing summary (must be preserved + merged)"
    // header so the compact model reliably folds them in rather than dropping them.
    let mut prior_summaries = String::new();
    let mut new_convo = String::new();
    for &i in &to_summarize_idx {
        let m = &messages[i];
        let is_summary = m.kind == Some(MessageKind::Summary);
        let role = match m.role {
            AgentMessageRole::Assistant => "assistant",
            AgentMessageRole::User => "user",
            AgentMessageRole::System => "system",
        };
        for b in &m.blocks {
            if let AgentMessageBlock::Text { text } = b {
                let target = if is_summary { &mut prior_summaries } else { &mut new_convo };
                if !is_summary {
                    target.push_str(role);
                    target.push_str(": ");
                }
                target.push_str(text);
                target.push('\n');
            }
        }
    }
    // Wrap in a clear "this is material to summarize — do NOT reply to it" envelope.
    // Without this, a transcript ending in a user turn tempts the model to ANSWER the
    // conversation instead of summarizing it. Prior summaries are demarcated so the model
    // folds (not drops) them into the new rolling summary.
    let material = if prior_summaries.trim().is_empty() {
        format!("【对话记录】\n{new_convo}")
    } else {
        format!(
            "【已有摘要——其中的事实必须完整保留并合并进新摘要，不得遗漏】\n{}\n\n【后续新增对话记录】\n{}",
            prior_summaries.trim_end(),
            new_convo
        )
    };
    let body = format!(
        "以下是需要你压缩成摘要的历史材料。请只输出摘要正文，**不要回复或回答材料中的任何问题**。\n\n{material}"
    );

    // One-shot collected call: build a transient HttpProvider over the compact channel.
    let mut provider = HttpProvider::new(compact_channel)?;
    let summarize_messages = vec![
        AgentMessage {
            message_id: "sum-sys".into(),
            run_id: Some(run_id.to_string()),
            conversation_id: None,
            seq: None,
            kind: None,
            role: AgentMessageRole::System,
            blocks: vec![AgentMessageBlock::Text {
                text: summarize_prompt.to_string(),
            }],
            created_at: Utc::now(),
        },
        AgentMessage {
            message_id: "sum-user".into(),
            run_id: Some(run_id.to_string()),
            conversation_id: None,
            seq: None,
            kind: None,
            role: AgentMessageRole::User,
            blocks: vec![AgentMessageBlock::Text { text: body }],
            created_at: Utc::now(),
        },
    ];
    let ctx = ContextBundle::new(run_id);
    // Throwaway sender: discard summary deltas so they don't leak to the run's event_tx.
    let (throwaway_tx, throwaway_rx) = mpsc::channel::<AgentEvent>(256);
    let drain = tokio::spawn(async move {
        let mut rx = throwaway_rx;
        while rx.recv().await.is_some() {}
    });
    let outcome = provider
        .next_turn(&summarize_messages, &ctx, &throwaway_tx, run_id)
        .await;
    drop(throwaway_tx);
    let _ = drain.await;
    let summary_text = outcome?.text;
    if summary_text.trim().is_empty() {
        return Ok(None);
    }

    Ok(Some(apply_summary(
        messages,
        &to_summarize_idx,
        summary_text,
        run_id,
    )))
}

/// 纯计算：把 `to_summarize_idx` 指向的消息替换为单条 `kind=Summary` durable 消息
/// （插在最旧那条的 Vec 位置）。
///
/// **seq 取被压缩区间的最大值（边界 seq），不是最旧那条**：这样持久化后该 summary 在审计序里
/// 紧贴「保留尾窗」之前，`load_conversation_view`（取最后一个 summary + 其后）才**有界**——只返回
/// 摘要 + 最近若干轮，而不是「摘要 + 其后全部历史」。若用最旧 seq，多轮滚动后 view 会随轮数线性膨胀。
/// 返回 (summary_msg, replaced_count)。
fn apply_summary(
    messages: &mut Vec<AgentMessage>,
    to_summarize_idx: &[usize],
    summary_text: String,
    run_id: &str,
) -> (AgentMessage, u32) {
    let first = to_summarize_idx[0];
    let last = *to_summarize_idx.last().expect("non-empty to_summarize_idx");
    let conversation_id = messages.get(first).and_then(|m| m.conversation_id.clone());
    // 边界 seq = 被压缩区间里最新一条的 seq（messages 按 seq 升序，故 last 索引即最大 seq）。
    let seq = messages.get(last).and_then(|m| m.seq);
    let summary_msg = AgentMessage {
        message_id: format!("sum-{}", Uuid::new_v4()),
        run_id: Some(run_id.to_string()),
        conversation_id,
        seq,
        kind: Some(MessageKind::Summary),
        role: AgentMessageRole::Assistant,
        blocks: vec![AgentMessageBlock::Text { text: summary_text }],
        created_at: Utc::now(),
    };
    let replaced = to_summarize_idx.len() as u32;
    for &i in to_summarize_idx.iter().rev() {
        messages.remove(i);
    }
    messages.insert(first, summary_msg.clone());
    (summary_msg, replaced)
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
    let token_estimate = prompt.chars().count().div_ceil(4) as u32;
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
    /// emit clean TextDelta from the SkillCallParser over the scripted text (mirrors
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
            input: vec![],
            conversation_id: None,
            compaction: None,
            fallback_channels: vec![],
            retry: None,
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
            run_agent_turn(req(), registry, ContextBundle::new("r1"), vec![provider], tx, None)
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
            run_agent_turn(req(), registry, ContextBundle::new("r1"), vec![provider], tx, None)
                .await
                .unwrap();
        assert_eq!(summary.stop_reason, AgentStopReason::Completed);
        assert_eq!(summary.turns, 2);
        assert_eq!(summary.skill_call_ids.len(), 1);

        // a `<use_skill>` turn emits clean TextDelta (raw XML suppressed) exactly
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
            run_agent_turn(req(), registry, ContextBundle::new("r1"), vec![provider], tx, None)
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
            run_agent_turn(req(), registry, ContextBundle::new("r1"), vec![provider], tx, None)
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
            run_agent_turn(req(), registry, ContextBundle::new("r1"), vec![provider], tx, None)
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

    // ---- §4 容错：瞬时退避重试 + 渠道 fallback 单测 ----------------------------------------
    //
    // `FlakyProvider` 按调用次序弹出预设 outcome（Ok 成功 / Err 各类错误），并用一个共享的
    // `Arc<Mutex<usize>>` 记录被调用次数，便于测试断言 retry / fallback 是否按预期发生。
    // 所有 retry 测试都把 `base_backoff_ms`/`max_backoff_ms` 设 0，避免真 sleep（hermetic）。
    struct FlakyProvider {
        outcomes: Arc<std::sync::Mutex<Vec<Result<ProviderTurnOutcome, LoopError>>>>,
        idx: Arc<std::sync::Mutex<usize>>,
        calls: Arc<std::sync::Mutex<usize>>,
    }
    impl FlakyProvider {
        fn new(outcomes: Vec<Result<ProviderTurnOutcome, LoopError>>) -> (Self, Arc<std::sync::Mutex<usize>>) {
            let calls = Arc::new(std::sync::Mutex::new(0usize));
            let p = FlakyProvider {
                outcomes: Arc::new(std::sync::Mutex::new(outcomes)),
                idx: Arc::new(std::sync::Mutex::new(0usize)),
                calls: Arc::clone(&calls),
            };
            (p, calls)
        }
    }
    #[async_trait::async_trait]
    impl ProviderStream for FlakyProvider {
        async fn next_turn(
            &mut self,
            _messages: &[AgentMessage],
            _context: &ContextBundle,
            _event_tx: &Sender<AgentEvent>,
            _run_id: &str,
        ) -> Result<ProviderTurnOutcome, LoopError> {
            *self.calls.lock().unwrap() += 1;
            let mut idx = self.idx.lock().unwrap();
            let mut outcomes = self.outcomes.lock().unwrap();
            if *idx >= outcomes.len() {
                return Err(LoopError::Provider("flaky ran out".into()));
            }
            // LoopError !Clone → swap out the slot to take ownership.
            let item = std::mem::replace(
                &mut outcomes[*idx],
                Err(LoopError::Provider("consumed".into())),
            );
            *idx += 1;
            item
        }
    }

    fn ok_outcome() -> Result<ProviderTurnOutcome, LoopError> {
        Ok(scripted_outcome("ok", 1, 1, AgentStopReason::Completed))
    }

    /// retry 配置：base/cap=0 → 无真 sleep。
    fn fast_retry(max_attempts: u32) -> crate::domain::agent::RetryConfig {
        crate::domain::agent::RetryConfig {
            max_attempts_per_channel: Some(max_attempts),
            base_backoff_ms: Some(0),
            max_backoff_ms: Some(0),
        }
    }

    #[tokio::test]
    async fn retry_succeeds_after_transient_errors() {
        let registry = Arc::new(SkillRegistry::new_without_persist());
        let (provider, calls) = FlakyProvider::new(vec![
            Err(LoopError::ProviderTransient("upstream".into())),
            Err(LoopError::ProviderTransient("upstream".into())),
            ok_outcome(),
        ]);
        let mut request = req();
        request.max_turns = 1;
        request.retry = Some(fast_retry(3));
        let (tx, _rx) = mpsc::channel::<AgentEvent>(64);
        let summary = run_agent_turn(
            request,
            registry,
            ContextBundle::new("r1"),
            vec![Box::new(provider)],
            tx,
            None,
        )
        .await
        .unwrap();
        assert_eq!(summary.stop_reason, AgentStopReason::Completed);
        assert_eq!(*calls.lock().unwrap(), 3, "should retry twice then succeed");
    }

    #[tokio::test]
    async fn fallback_to_second_channel_when_primary_exhausts() {
        let registry = Arc::new(SkillRegistry::new_without_persist());
        let (primary, primary_calls) = FlakyProvider::new(vec![
            Err(LoopError::ProviderTransient("p1".into())),
            Err(LoopError::ProviderTransient("p1".into())),
        ]);
        let (secondary, secondary_calls) = FlakyProvider::new(vec![ok_outcome()]);
        let mut request = req();
        request.max_turns = 1;
        request.retry = Some(fast_retry(2));
        let (tx, _rx) = mpsc::channel::<AgentEvent>(64);
        let summary = run_agent_turn(
            request,
            registry,
            ContextBundle::new("r1"),
            vec![Box::new(primary), Box::new(secondary)],
            tx,
            None,
        )
        .await
        .unwrap();
        assert_eq!(summary.stop_reason, AgentStopReason::Completed);
        assert_eq!(*primary_calls.lock().unwrap(), 2, "primary tried max_attempts then gave up");
        assert_eq!(*secondary_calls.lock().unwrap(), 1, "fallback channel handled it once");
    }

    #[tokio::test]
    async fn fatal_error_no_retry_no_fallback() {
        let registry = Arc::new(SkillRegistry::new_without_persist());
        let (primary, primary_calls) =
            FlakyProvider::new(vec![Err(LoopError::Provider("400 bad request".into()))]);
        let (secondary, secondary_calls) = FlakyProvider::new(vec![ok_outcome()]);
        let mut request = req();
        request.max_turns = 1;
        request.retry = Some(fast_retry(3));
        let (tx, _rx) = mpsc::channel::<AgentEvent>(64);
        let res = run_agent_turn(
            request,
            registry,
            ContextBundle::new("r1"),
            vec![Box::new(primary), Box::new(secondary)],
            tx,
            None,
        )
        .await;
        assert!(matches!(res, Err(LoopError::Provider(_))), "fatal error propagates");
        assert_eq!(*primary_calls.lock().unwrap(), 1, "fatal → no retry");
        assert_eq!(*secondary_calls.lock().unwrap(), 0, "fatal → no fallback");
    }

    #[tokio::test]
    async fn context_too_long_not_treated_as_transient() {
        // ProviderContextTooLong must NOT enter the transient backoff/retry path. It is handled by
        // the orthogonal reactive-retry path: ONE compaction + ONE resend, then fail closed.
        // Three consecutive CTLs discriminate the two behaviours:
        //   correct  → call#1 CTL → compact+resend → call#2 CTL → fail closed = EXACTLY 2 calls,
        //              stop_reason = ContextLimit.
        //   regressed (CTL treated as transient) → 3 backoff retries = 3 calls (then Err → panic).
        let registry = Arc::new(SkillRegistry::new_without_persist());
        let (provider, calls) = FlakyProvider::new(vec![
            Err(LoopError::ProviderContextTooLong),
            Err(LoopError::ProviderContextTooLong),
            Err(LoopError::ProviderContextTooLong),
        ]);
        let mut request = req();
        request.max_turns = 2;
        request.retry = Some(fast_retry(3));
        let (tx, _rx) = mpsc::channel::<AgentEvent>(64);
        let summary = run_agent_turn(
            request,
            registry,
            ContextBundle::new("r1"),
            vec![Box::new(provider)],
            tx,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            summary.stop_reason,
            AgentStopReason::ContextLimit,
            "two CTLs must fail closed via reactive retry, not loop forever"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            2,
            "context-too-long → reactive retry (1 compact + 1 resend) = exactly 2 calls; \
             a transient-storm regression would make 3"
        );
    }

    #[tokio::test]
    async fn retry_exhausted_all_channels_fails() {
        let registry = Arc::new(SkillRegistry::new_without_persist());
        let (p1, p1_calls) = FlakyProvider::new(vec![
            Err(LoopError::ProviderTransient("p1".into())),
            Err(LoopError::ProviderTransient("p1".into())),
        ]);
        let (p2, p2_calls) = FlakyProvider::new(vec![
            Err(LoopError::ProviderTransient("p2".into())),
            Err(LoopError::ProviderTransient("p2".into())),
        ]);
        let mut request = req();
        request.max_turns = 1;
        request.retry = Some(fast_retry(2));
        let (tx, _rx) = mpsc::channel::<AgentEvent>(64);
        let res = run_agent_turn(
            request,
            registry,
            ContextBundle::new("r1"),
            vec![Box::new(p1), Box::new(p2)],
            tx,
            None,
        )
        .await;
        assert!(
            matches!(res, Err(LoopError::ProviderTransient(_))),
            "all channels exhausted → ProviderTransient"
        );
        assert_eq!(*p1_calls.lock().unwrap(), 2, "p1 tried max_attempts");
        assert_eq!(*p2_calls.lock().unwrap(), 2, "p2 tried max_attempts");
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
        let _ = run_agent_turn(req(), registry, ContextBundle::new("r1"), vec![provider], tx, None)
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
        let _ = run_agent_turn(req(), registry, ContextBundle::new("r1"), vec![provider], tx, None)
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
            run_agent_turn(req(), registry, ContextBundle::new("r1"), vec![provider], tx, None)
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

    // ---- multi-turn persistence + proactive compaction (deterministic) ----

    fn fresh_repo() -> AgentMessagesRepo {
        use crate::infrastructure::agent::migrations::migrations as agent_migrations;
        use crate::infrastructure::db::{run_migrations, AppDb};
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        AgentMessagesRepo::new(db)
    }

    /// New contract (spec §4): the engine itself persists both the round's `input` (the new user
    /// message) AND its own produced assistant output under the conversation_id, assigning seq.
    /// The caller only supplies `input=[user]` + `conversation_id` + `Some(repo)` — never upserts
    /// or assigns seq itself.
    #[tokio::test]
    async fn run_agent_turn_persists_and_reloads_view() {
        let repo = fresh_repo();
        let registry = Arc::new(SkillRegistry::new_without_persist());
        let provider = Box::new(ScriptedProvider {
            script: vec![Ok(scripted_outcome(
                "first answer",
                3,
                4,
                AgentStopReason::Completed,
            ))],
            index: 0,
        });
        let mut request = req();
        request.conversation_id = Some("conv-x".into());
        // Only the new user message — engine persists it (assigns conversation_id + seq) then
        // persists the assistant output it produces.
        request.input = vec![AgentMessage {
            message_id: "u-1".into(),
            run_id: Some("r1".into()),
            conversation_id: None,
            seq: None,
            kind: None,
            role: AgentMessageRole::User,
            blocks: vec![AgentMessageBlock::Text {
                text: "hello".into(),
            }],
            created_at: Utc::now(),
        }];
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        let summary = run_agent_turn(
            request,
            registry,
            ContextBundle::new("r1"),
            vec![provider],
            tx,
            Some(repo.clone()),
        )
        .await
        .unwrap();
        assert_eq!(summary.stop_reason, AgentStopReason::Completed);
        while rx.recv().await.is_some() {}

        // Engine persisted BOTH the input user message and the produced assistant message.
        let all = repo.load_conversation("conv-x").unwrap();
        assert_eq!(all.len(), 2, "engine persists input + produced output");
        // All carry conversation_id, monotonic seq starting at 0, default kind=Chat.
        assert!(all.iter().all(|m| m.conversation_id.as_deref() == Some("conv-x")));
        assert_eq!(all[0].seq, Some(0));
        assert_eq!(all[1].seq, Some(1));
        assert!(all[0].seq < all[1].seq, "seq monotonic");
        assert!(all.iter().all(|m| m.kind == Some(MessageKind::Chat)));
        // Engine persisted input first (user) then output (assistant).
        assert_eq!(all[0].role, AgentMessageRole::User);
        assert_eq!(all[1].role, AgentMessageRole::Assistant);
        // No summary checkpoint yet → view == full.
        let view = repo.load_conversation_view("conv-x").unwrap();
        assert_eq!(view.len(), 2);
    }

    /// New contract (spec §4): given a conversation that already has a summary checkpoint (fixture
    /// representing prior persisted rounds), the caller passes only this round's new `input`. The
    /// engine persists that input, then loads the *compressed* view (summary + post-summary tail +
    /// the just-persisted input) as the turn context — the caller never pre-loads the view.
    #[tokio::test]
    async fn run_agent_turn_loads_compressed_view_plus_persisted_input() {
        let repo = fresh_repo();
        // Fixture: prior persisted state — an old message, a Summary checkpoint, a recent message.
        let mk = |id: &str, seq: i64, kind: Option<MessageKind>, text: &str| AgentMessage {
            message_id: id.into(),
            run_id: Some("r0".into()),
            conversation_id: Some("c".into()),
            seq: Some(seq),
            kind,
            role: AgentMessageRole::Assistant,
            blocks: vec![AgentMessageBlock::Text { text: text.into() }],
            created_at: Utc::now(),
        };
        repo.upsert_message(&mk("m0", 0, None, "old")).unwrap();
        repo.upsert_message(&mk("s1", 1, Some(MessageKind::Summary), "SUMMARY"))
            .unwrap();
        repo.upsert_message(&mk("m2", 2, None, "recent")).unwrap();

        // SnapshotProvider records the messages the loop seeded the turn with.
        struct SeedSnapshot {
            seen: Arc<std::sync::Mutex<Vec<AgentMessage>>>,
        }
        #[async_trait::async_trait]
        impl ProviderStream for SeedSnapshot {
            async fn next_turn(
                &mut self,
                messages: &[AgentMessage],
                _c: &ContextBundle,
                _tx: &Sender<AgentEvent>,
                _r: &str,
            ) -> Result<ProviderTurnOutcome, LoopError> {
                *self.seen.lock().unwrap() = messages.to_vec();
                Ok(ProviderTurnOutcome {
                    text: "ok".into(),
                    usage_input: 1,
                    usage_output: 1,
                    stop_reason: AgentStopReason::Completed,
                    skill_events: Vec::new(),
                })
            }
        }
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Box::new(SeedSnapshot {
            seen: Arc::clone(&seen),
        });
        let registry = Arc::new(SkillRegistry::new_without_persist());
        let mut request = req();
        request.conversation_id = Some("c".into());
        // Only the new user input for this round — engine persists it, then loads the view.
        request.input = vec![AgentMessage {
            message_id: "u-new".into(),
            run_id: Some("r1".into()),
            conversation_id: None,
            seq: None,
            kind: None,
            role: AgentMessageRole::User,
            blocks: vec![AgentMessageBlock::Text {
                text: "follow up".into(),
            }],
            created_at: Utc::now(),
        }];
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        let _ = run_agent_turn(
            request,
            registry,
            ContextBundle::new("r1"),
            vec![provider],
            tx,
            Some(repo.clone()),
        )
        .await
        .unwrap();
        while rx.recv().await.is_some() {}

        // Engine persisted the input (assigning conversation_id + seq=3) — m0 stays elided by the
        // compressed view, so the turn saw: summary s1 + recent m2 + the just-persisted input.
        let persisted_input = repo.load_conversation("c").unwrap();
        assert!(
            persisted_input.iter().any(|m| m.message_id == "u-new"
                && m.conversation_id.as_deref() == Some("c")
                && m.seq == Some(3)),
            "engine persisted the new input with conversation_id + monotonic seq"
        );
        let seeded = seen.lock().unwrap().clone();
        let ids: Vec<&str> = seeded.iter().map(|m| m.message_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["s1", "m2", "u-new"],
            "turn seeded from compressed view (summary + tail) plus the persisted input"
        );
    }

    #[tokio::test]
    async fn proactive_compaction_micro_clears_old_skill_results_at_threshold() {
        // Tiny window forces proactive compaction. Seed a large droppable realtime skill_result
        // part; first turn's pre-compaction MicroClear should stub it.
        let registry = Arc::new(SkillRegistry::new_without_persist());
        let provider = Box::new(ScriptedProvider {
            script: vec![Ok(scripted_outcome("done", 1, 1, AgentStopReason::Completed))],
            index: 0,
        });
        let mut request = req();
        // Tiny window: soft = 200/2 = 100, summarize = 140, hard = 180.
        request.channel.context_window_tokens = Some(200);
        let mut ctx = ContextBundle::new("r1");
        ctx.realtime_parts.push(ContextPart {
            kind: ContextPartKind::Realtime,
            content: ContextContent::Text(format!(
                r#"<skill_result name="q" call_id="sc_q" ref="pl_q">{}</skill_result>"#,
                "q".repeat(2000)
            )),
            freshness: None,
            token_estimate: None,
            droppable: true,
        });
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
        let _ = run_agent_turn(request, registry, ctx, vec![provider], tx, None)
            .await
            .unwrap();
        let mut saw_micro = false;
        while let Some(e) = rx.recv().await {
            if let AgentEvent::Compacted { tier, .. } = e {
                if matches!(tier, CompactedTier::MicroClear) {
                    saw_micro = true;
                }
            }
        }
        assert!(saw_micro, "proactive MicroClear should fire above soft limit");
    }

    #[tokio::test]
    async fn proactive_compaction_drops_when_no_summarize_prompt() {
        // Above summarize threshold, with no summarize_prompt → Summarize degrades to Drop.
        let registry = Arc::new(SkillRegistry::new_without_persist());
        let provider = Box::new(ScriptedProvider {
            script: vec![Ok(scripted_outcome("done", 1, 1, AgentStopReason::Completed))],
            index: 0,
        });
        let mut request = req();
        request.channel.context_window_tokens = Some(200); // soft 100 / summarize 140 / hard 180
        // Many old non-durable assistant messages (well past keep_recent) → big + droppable.
        let mut seed = Vec::new();
        for i in 0..8 {
            seed.push(AgentMessage {
                message_id: format!("seed-{i}"),
                run_id: Some("r1".into()),
                conversation_id: None,
                seq: None,
                kind: None,
                role: AgentMessageRole::Assistant,
                blocks: vec![AgentMessageBlock::Text {
                    text: "x".repeat(400),
                }],
                created_at: Utc::now(),
            });
        }
        request.input = seed;
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
        let _ = run_agent_turn(request, registry, ContextBundle::new("r1"), vec![provider], tx, None)
            .await
            .unwrap();
        let mut saw_drop = false;
        while let Some(e) = rx.recv().await {
            if let AgentEvent::Compacted { tier, .. } = e {
                if matches!(tier, CompactedTier::Drop) {
                    saw_drop = true;
                }
            }
        }
        assert!(saw_drop, "no summarize_prompt → Drop oldest round");
    }

    #[test]
    fn apply_summary_replaces_turns_with_single_summary_message() {
        let mut msgs = vec![
            AgentMessage {
                message_id: "a0".into(),
                run_id: Some("r".into()),
                conversation_id: Some("c".into()),
                seq: Some(0),
                kind: Some(MessageKind::Chat),
                role: AgentMessageRole::Assistant,
                blocks: vec![AgentMessageBlock::Text { text: "old0".into() }],
                created_at: Utc::now(),
            },
            AgentMessage {
                message_id: "u1".into(),
                run_id: Some("r".into()),
                conversation_id: Some("c".into()),
                seq: Some(1),
                kind: Some(MessageKind::Chat),
                role: AgentMessageRole::User,
                blocks: vec![AgentMessageBlock::Text { text: "old1".into() }],
                created_at: Utc::now(),
            },
            AgentMessage {
                message_id: "a2".into(),
                run_id: Some("r".into()),
                conversation_id: Some("c".into()),
                seq: Some(2),
                kind: Some(MessageKind::Chat),
                role: AgentMessageRole::Assistant,
                blocks: vec![AgentMessageBlock::Text { text: "recent".into() }],
                created_at: Utc::now(),
            },
        ];
        let (summary, replaced) =
            apply_summary(&mut msgs, &[0, 1], "SUMMARY TEXT".into(), "r");
        assert_eq!(replaced, 2);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].message_id, summary.message_id);
        assert_eq!(msgs[0].kind, Some(MessageKind::Summary));
        assert_eq!(
            msgs[0].seq,
            Some(1),
            "summary takes the boundary (newest summarized) seq, not the oldest — keeps load_conversation_view bounded"
        );
        assert_eq!(msgs[1].message_id, "a2", "recent message kept");
    }

    /// 端到端实网 chat：跑完整 `run_agent_turn`（loop + HttpProvider + 真 SSE）对三个真实
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
                input: vec![AgentMessage {
                    message_id: "m1".into(),
                    run_id: Some("live".into()),
                    conversation_id: None,
                    seq: None,
                    kind: None,
                    role: AgentMessageRole::User,
                    blocks: vec![AgentMessageBlock::Text {
                        text: "用一句话解释A股的T+1交易制度".into(),
                    }],
                    created_at: Utc::now(),
                }],
                conversation_id: None,
                compaction: None,
                fallback_channels: vec![],
                retry: None,
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
                run_agent_turn(request, registry, ContextBundle::new("live"), vec![provider], tx, None)
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

    /// 端到端实网 SKILL：注册一个 `get_secret` skill（只有工具知道答案），让真实模型
    /// 通过 `<use_skill>` 文本协议调用它、拿到结果、再据此作答。验证整条 skill 回路
    /// （SystemPromptBuilder 注入清单 → 模型发 use_skill → dispatch → skill_result 回灌 →
    /// 模型用结果作答）。`#[ignore]`，凭证走 env。
    #[tokio::test]
    #[ignore]
    async fn skill_loop_live() {
        use crate::infrastructure::agent::http_provider::HttpProvider;
        use crate::infrastructure::agent::skill_registry::SkillRegistry;
        use chrono::Utc;
        use serde_json::json;

        async fn one(label: &str, wire: WireFormat, base: String, key: String, model: String) {
            let registry = Arc::new(SkillRegistry::new_without_persist());
            let secret_spec = SkillSpec::new(
                "get_secret",
                "返回今天的幸运数字（一个整数）。当用户问幸运数字时必须调用本 skill 获取，不要自己编。",
                json!({"type":"object","properties":{}}),
                vec![r#"<use_skill name="get_secret">{}</use_skill>"#.to_string()],
                5000,
                SideEffect::None,
            );
            let handler: Arc<dyn SkillHandler> = Arc::new(FnSkillHandler(|_inv: SkillInvocation| {
                Box::pin(async move { SkillHandlerOutput::ok(json!({"secret": 4242})) })
                    as SkillHandlerFuture
            }));
            registry.register_skill(secret_spec, handler).unwrap();

            let mut ch = channel();
            ch.wire_format = wire;
            ch.base_url = Some(base);
            ch.api_key = key;
            ch.model = model;
            ch.max_output_tokens = Some(1024);
            let provider = Box::new(HttpProvider::new(ch.clone()).unwrap());
            let request = AgentRunRequest {
                run_id: "skill".into(),
                trigger: "user".into(),
                channel: ch,
                max_turns: 4,
                input: vec![AgentMessage {
                    message_id: "m1".into(),
                    run_id: Some("skill".into()),
                    conversation_id: None,
                    seq: None,
                    kind: None,
                    role: AgentMessageRole::User,
                    blocks: vec![AgentMessageBlock::Text {
                        text: "请调用 get_secret 这个 skill 获取今天的幸运数字，然后用一句话告诉我它是多少。".into(),
                    }],
                    created_at: Utc::now(),
                }],
                conversation_id: None,
                compaction: None,
                fallback_channels: vec![],
                retry: None,
            };
            let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
            let pump = tokio::spawn(async move {
                let mut text = String::new();
                let mut skills: Vec<(String, bool)> = Vec::new();
                while let Some(e) = rx.recv().await {
                    match e {
                        AgentEvent::TextDelta { delta, .. } => text.push_str(&delta),
                        AgentEvent::SkillEnd { name, is_error, .. } => skills.push((name, is_error)),
                        _ => {}
                    }
                }
                (text, skills)
            });
            let summary =
                run_agent_turn(request, registry, ContextBundle::new("skill"), vec![provider], tx, None)
                    .await
                    .unwrap_or_else(|e| panic!("[{label}] loop failed: {e}"));
            let (text, skills) = pump.await.unwrap();
            println!(
                "[skill-live][{label}] stop={:?} skills={:?} answer={:?}",
                summary.stop_reason, skills, text
            );
            assert!(
                skills.iter().any(|(n, err)| n == "get_secret" && !err),
                "[{label}] get_secret skill was not dispatched successfully"
            );
            assert!(text.contains("4242"), "[{label}] final answer didn't use the skill result");
        }

        let mut ran = 0;
        // 用快的两家测 skill（deepseek + anthropic）；gpt-5 太慢，按需自行加 TEST_OAI_*。
        if let Ok(k) = std::env::var("TEST_DS_KEY") {
            let b = std::env::var("TEST_DS_BASE").unwrap_or_else(|_| "https://api.deepseek.com".into());
            let m = std::env::var("TEST_DS_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".into());
            one("chat_completions", WireFormat::ChatCompletions, b, k, m).await;
            ran += 1;
        }
        if let (Ok(b), Ok(k)) = (std::env::var("TEST_ANT_BASE"), std::env::var("TEST_ANT_KEY")) {
            let m = std::env::var("TEST_ANT_MODEL")
                .unwrap_or_else(|_| "claude-haiku-4-5-20251001".into());
            one("messages", WireFormat::Messages, b, k, m).await;
            ran += 1;
        }
        if let (Ok(b), Ok(k)) = (std::env::var("TEST_OAI_BASE"), std::env::var("TEST_OAI_KEY")) {
            let m = std::env::var("TEST_OAI_MODEL").unwrap_or_else(|_| "gpt-5".into());
            one("responses", WireFormat::Responses, b, k, m).await;
            ran += 1;
        }
        println!("[skill-live] ran {ran} skill-loop checks");
        assert!(ran > 0, "no TEST_* env provided");
    }

    /// 实网多轮 + Summarize 烟测：
    /// (a) 一个短多轮会话持久化到 in-memory DB（一个 conversationId），reload 压缩视图；
    /// (b) 用极小 summarize_threshold + 一个 summarizePrompt 强制触发 Summarize，断言产出一条
    ///     `kind=Summary` 消息且模型调用成功。
    /// `#[ignore]`，凭证全走 env（无硬编码 secret）：
    ///   TEST_ANT_BASE/KEY（messages）或 TEST_DS_KEY（chat_completions）。
    #[tokio::test]
    #[ignore]
    async fn multiturn_summarize_live() {
        use crate::infrastructure::agent::http_provider::HttpProvider;
        use crate::infrastructure::agent::migrations::migrations as agent_migrations;
        use crate::infrastructure::db::{run_migrations, AppDb};
        use chrono::Utc;

        // Pick a live channel from env (prefer DeepSeek chat_completions, else Anthropic messages).
        let channel = if let Ok(k) = std::env::var("TEST_DS_KEY") {
            let b =
                std::env::var("TEST_DS_BASE").unwrap_or_else(|_| "https://api.deepseek.com".into());
            let m = std::env::var("TEST_DS_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".into());
            let mut c = channel();
            c.wire_format = WireFormat::ChatCompletions;
            c.base_url = Some(b);
            c.api_key = k;
            c.model = m;
            c.max_output_tokens = Some(512);
            c
        } else if let (Ok(b), Ok(k)) =
            (std::env::var("TEST_ANT_BASE"), std::env::var("TEST_ANT_KEY"))
        {
            let m = std::env::var("TEST_ANT_MODEL")
                .unwrap_or_else(|_| "claude-haiku-4-5-20251001".into());
            let mut c = channel();
            c.wire_format = WireFormat::Messages;
            c.base_url = Some(b);
            c.api_key = k;
            c.model = m;
            c.max_output_tokens = Some(512);
            c
        } else {
            println!("[multiturn-summarize-live] skip: set TEST_DS_KEY or TEST_ANT_BASE/KEY");
            return;
        };

        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        let repo = AgentMessagesRepo::new(db);
        let conversation_id = "live-conv".to_string();

        // (a) Turn 1: a short multi-turn conversation persisted under the conversation_id.
        async fn user_seed(conv: &str, run: &str, text: &str) -> Vec<AgentMessage> {
            vec![AgentMessage {
                message_id: format!("u-{run}"),
                run_id: Some(run.to_string()),
                conversation_id: Some(conv.to_string()),
                seq: None,
                kind: Some(MessageKind::Chat),
                role: AgentMessageRole::User,
                blocks: vec![AgentMessageBlock::Text { text: text.into() }],
                created_at: Utc::now(),
            }]
        }

        // New contract: hand the engine only this round's new user input; it persists input +
        // produced output under conversation_id itself.
        let input1 = user_seed(&conversation_id, "run-1", "我关注贵州茅台(600519.SH)，简单说说它。").await;

        let registry = Arc::new(SkillRegistry::new_without_persist());
        let provider = Box::new(HttpProvider::new(channel.clone()).unwrap());
        let req1 = AgentRunRequest {
            run_id: "run-1".into(),
            trigger: "user".into(),
            channel: channel.clone(),
            max_turns: 1,
            input: input1,
            conversation_id: Some(conversation_id.clone()),
            compaction: None,
            fallback_channels: vec![],
            retry: None,
        };
        let (tx1, mut rx1) = mpsc::channel::<AgentEvent>(256);
        let pump1 = tokio::spawn(async move { while rx1.recv().await.is_some() {} });
        let s1 = run_agent_turn(
            req1,
            registry.clone(),
            ContextBundle::new("run-1"),
            vec![provider],
            tx1,
            Some(repo.clone()),
        )
        .await
        .expect("turn 1 loop");
        pump1.await.unwrap();
        println!("[multiturn-summarize-live] turn1 stop={:?}", s1.stop_reason);

        // Reload the compressed view (no summary yet → full).
        let view = repo.load_conversation_view(&conversation_id).unwrap();
        assert!(!view.is_empty(), "conversation persisted + reloadable");
        println!("[multiturn-summarize-live] view len after turn1 = {}", view.len());

        // (b) Turn 2: force Summarize with a tiny summarize_threshold + a summarizePrompt.
        // Engine loads turn-1 history (persisted above) + persists this round's new input itself.
        let input2 = user_seed(&conversation_id, "run-2", "它的护城河主要是什么？").await;

        let provider2 = Box::new(HttpProvider::new(channel.clone()).unwrap());
        let req2 = AgentRunRequest {
            run_id: "run-2".into(),
            trigger: "user".into(),
            channel: channel.clone(),
            max_turns: 1,
            input: input2,
            conversation_id: Some(conversation_id.clone()),
            compaction: Some(CompactionConfig {
                soft_limit_tokens: Some(1),
                summarize_threshold_tokens: Some(1),
                hard_limit_tokens: Some(1_000_000),
                keep_recent_turns: Some(1),
                summarize_prompt: Some(
                    "你是会话压缩器。用中文把以下对话压缩成一段要点摘要，覆盖关注标的与已建立的判断。仅输出摘要正文。"
                        .into(),
                ),
                compact_channel: None,
            }),
            fallback_channels: vec![],
            retry: None,
        };
        let (tx2, mut rx2) = mpsc::channel::<AgentEvent>(256);
        let pump2 = tokio::spawn(async move {
            let mut tiers = Vec::new();
            while let Some(e) = rx2.recv().await {
                if let AgentEvent::Compacted { tier, .. } = e {
                    tiers.push(tier);
                }
            }
            tiers
        });
        let s2 = run_agent_turn(
            req2,
            registry,
            ContextBundle::new("run-2"),
            vec![provider2],
            tx2,
            Some(repo.clone()),
        )
        .await
        .expect("turn 2 loop");
        let tiers = pump2.await.unwrap();
        println!(
            "[multiturn-summarize-live] turn2 stop={:?} compaction_tiers={:?}",
            s2.stop_reason, tiers
        );
        assert!(
            tiers.iter().any(|t| matches!(t, CompactedTier::Summarize)),
            "expected a Summarize compaction tier"
        );

        // A kind=Summary message must now exist in the conversation (persisted).
        let all = repo.load_conversation(&conversation_id).unwrap();
        let n_summary = all
            .iter()
            .filter(|m| m.kind == Some(MessageKind::Summary))
            .count();
        println!(
            "[multiturn-summarize-live] total persisted = {}, summary checkpoints = {}",
            all.len(),
            n_summary
        );
        assert!(n_summary >= 1, "a Summary checkpoint must be produced");
        let summary_text = all
            .iter()
            .find(|m| m.kind == Some(MessageKind::Summary))
            .and_then(|m| m.blocks.first())
            .map(|b| match b {
                AgentMessageBlock::Text { text } => text.clone(),
                _ => String::new(),
            })
            .unwrap_or_default();
        assert!(!summary_text.trim().is_empty(), "summary model call must produce text");
        println!("[multiturn-summarize-live] summary = {:?}", summary_text);
    }
}
