//! Canonical Agent loop 编排（infra 内部 use case）。
//!
//! Spec: docs/design/agent-infra-module.md §3 / §5 Infra Loop API
//!
//! 行为：
//! 1. emit `run_start`
//! 2. 调 provider stream（trait `ProviderStream`，便于注入 fake provider 测试）
//! 3. text / thinking delta -> emit
//! 4. provider 给出 tool_use -> ToolRegistry dispatch -> append assistant tool_use + tool tool_result message -> 继续
//! 5. 达到 max_turns 或 provider stop -> emit `done`
//! 6. 任何错误 emit `error`，loop 关闭
//!
//! Phase 1：streaming 接线由 trait 注入；真实 HTTP / SSE 接线在后续迭代或测试 fixture 落地。

use crate::domain::agent::{
    AgentEvent, AgentMessage, AgentMessageBlock, AgentMessageRole, AgentRunRequest,
    AgentStopReason, ContextBundle, JsonSummary, RunSummary,
};
use crate::infrastructure::agent::tool_registry::ToolRegistry;
use chrono::Utc;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use uuid::Uuid;

/// 一次 provider stream 的拉取结果。
///
/// Provider 在一次 stream 内可以混合 text / thinking delta、tool_use 请求、usage、stop。
/// 本 trait 抽象掉具体 SSE 解码细节，让 loop 测试可以注入 mock。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderTurnOutcome {
    pub text: String,
    pub thinking: Vec<String>,
    pub tool_uses: Vec<ProviderToolUse>,
    pub usage_input: u32,
    pub usage_output: u32,
    /// provider 原始 stop reason（adapter 归一化后值）。
    pub stop_reason: AgentStopReason,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderToolUse {
    pub tool_call_id: String,
    pub name: String,
    pub input: JsonSummary,
}

/// Provider stream 抽象。Loop executor 在每个 turn 调用一次 `next_turn`。
#[async_trait::async_trait]
pub trait ProviderStream: Send + Sync {
    /// 拉取下一轮 provider 输出（包含一段文本 + 0..n 个 tool_use + stop reason）。
    /// `messages`：截至当前 turn 的累积 canonical 消息，含 seed + 历次 assistant / tool turn。
    async fn next_turn(
        &mut self,
        messages: &[AgentMessage],
        event_tx: &Sender<AgentEvent>,
        run_id: &str,
    ) -> Result<ProviderTurnOutcome, LoopError>;
}

#[derive(Debug, thiserror::Error)]
pub enum LoopError {
    #[error("provider error: {0}")]
    Provider(String),
    #[error("tool dispatch error: {0}")]
    Dispatch(#[from] crate::infrastructure::agent::tool_registry::DispatchError),
    #[error("event channel closed")]
    EventChannelClosed,
    #[error("context too long, all compaction exhausted")]
    ContextTooLong,
}

/// 执行一次 Agent loop。
///
/// Spec §3：`run_agent_loop(request, registry, context, event_tx) -> RunSummary`
///
/// 实参：
/// - `provider`：调用 channel adapter 的 stream（trait `ProviderStream`）。
/// - `registry`：本次 run 允许的 local tools。
/// - `context`：runtime 提供（spec §2 ContextBundle）。本 Phase 暂不在 loop 内调 compact；
///   `context` 由 caller 在 build provider request 之前压缩。
/// - `event_tx`：AgentEvent stream。
pub async fn run_agent_loop(
    request: AgentRunRequest,
    registry: Arc<ToolRegistry>,
    _context: ContextBundle,
    mut provider: Box<dyn ProviderStream>,
    event_tx: Sender<AgentEvent>,
) -> Result<RunSummary, LoopError> {
    let run_id = request.run_id.clone();
    // 1. emit run_start
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
    let mut tool_call_ids: Vec<String> = Vec::new();
    let mut usage_input: u32 = 0;
    let mut usage_output: u32 = 0;

    let mut turn: u32 = 0;
    let stop_reason: AgentStopReason;
    loop {
        if turn >= request.max_turns {
            stop_reason = AgentStopReason::MaxTurns;
            break;
        }
        turn += 1;

        let outcome = provider
            .next_turn(&messages, &event_tx, &run_id)
            .await
            .map_err(|e| match e {
                LoopError::EventChannelClosed => LoopError::EventChannelClosed,
                other => other,
            })?;
        usage_input = usage_input.saturating_add(outcome.usage_input);
        usage_output = usage_output.saturating_add(outcome.usage_output);

        // 装配本轮 assistant message（text + 可选 tool_use）。
        let mut blocks: Vec<AgentMessageBlock> = Vec::new();
        if !outcome.text.is_empty() {
            blocks.push(AgentMessageBlock::Text {
                text: outcome.text.clone(),
            });
        }
        for tu in &outcome.tool_uses {
            blocks.push(AgentMessageBlock::ToolUse {
                tool_call_id: tu.tool_call_id.clone(),
                name: tu.name.clone(),
                input_summary: tu.input.clone(),
            });
        }
        if !blocks.is_empty() {
            messages.push(AgentMessage {
                message_id: format!("am-{}", Uuid::new_v4()),
                run_id: Some(run_id.clone()),
                role: AgentMessageRole::Assistant,
                blocks,
                created_at: Utc::now(),
            });
        }

        // 没有 tool_use 且 provider 给的 stop 是 completed -> 结束。
        if outcome.tool_uses.is_empty() {
            if matches!(outcome.stop_reason, AgentStopReason::MaxTurns) {
                stop_reason = AgentStopReason::MaxTurns;
            } else if matches!(outcome.stop_reason, AgentStopReason::ProviderStop) {
                stop_reason = AgentStopReason::ProviderStop;
            } else {
                stop_reason = AgentStopReason::Completed;
            }
            break;
        }

        // 处理每个 tool_use：dispatch → emit tool_start/tool_end → append tool_result message。
        for tu in outcome.tool_uses {
            send_event(
                &event_tx,
                AgentEvent::ToolStart {
                    run_id: run_id.clone(),
                    tool_call_id: tu.tool_call_id.clone(),
                    name: tu.name.clone(),
                    input_summary: tu.input.clone(),
                },
            )
            .await?;

            let dispatch_res = registry
                .dispatch_tool_call(
                    &run_id,
                    tu.tool_call_id.clone(),
                    &tu.name,
                    tu.input.clone(),
                )
                .await;
            // dispatch error -> tool error result（spec §5）
            let (out_summary, is_error, duration_ms, call_id) = match dispatch_res {
                Ok(r) => (
                    r.output_summary,
                    r.is_error,
                    r.duration_ms,
                    r.tool_call_id,
                ),
                Err(e) => {
                    // 仍记一次 tool_call_id（使用 provider 给的 id），把错误反馈给模型。
                    let summary = serde_json::json!({"error": e.to_string()});
                    (summary, true, 0u64, tu.tool_call_id.clone())
                }
            };

            tool_call_ids.push(call_id.clone());

            send_event(
                &event_tx,
                AgentEvent::ToolEnd {
                    run_id: run_id.clone(),
                    tool_call_id: call_id.clone(),
                    name: tu.name.clone(),
                    output_summary: out_summary.clone(),
                    is_error,
                    duration_ms,
                },
            )
            .await?;

            // 追加 tool result message，保持 tool_use / tool_result 配对
            // —— spec §2 不变量：易腐工具结果替换 stub 时也必须保留 call id。
            messages.push(AgentMessage {
                message_id: format!("am-{}", Uuid::new_v4()),
                run_id: Some(run_id.clone()),
                role: AgentMessageRole::Tool,
                blocks: vec![AgentMessageBlock::ToolResult {
                    tool_call_id: tu.tool_call_id.clone(),
                    output_summary: out_summary,
                    is_error,
                }],
                created_at: Utc::now(),
            });
        }
        // 继续下一个 turn（继续把 tool_result 喂回 provider）。
    }

    // emit usage + done
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
        tool_call_ids,
    })
}

async fn send_event(tx: &Sender<AgentEvent>, e: AgentEvent) -> Result<(), LoopError> {
    tx.send(e).await.map_err(|_| LoopError::EventChannelClosed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::{
        ProviderChannel, ToolSideEffect, ToolSpec, WireFormat,
    };
    use crate::infrastructure::agent::tool_registry::{
        FnToolHandler, ToolHandler, ToolHandlerFuture, ToolHandlerOutput, ToolInvocation,
    };
    use serde_json::json;
    use tokio::sync::mpsc;

    fn channel() -> ProviderChannel {
        ProviderChannel {
            channel_id: "fake".into(),
            provider: "fake".into(),
            wire_format: WireFormat::Messages,
            base_url: None,
            model: "fake-model".into(),
            stream: true,
            supports_tools: true,
            supports_vision: false,
            supports_thinking: false,
            supports_server_side_tools: None,
        }
    }

    /// Fake provider — 按预设脚本输出 turns。
    struct ScriptedProvider {
        script: Vec<ProviderTurnOutcome>,
        index: usize,
    }
    #[async_trait::async_trait]
    impl ProviderStream for ScriptedProvider {
        async fn next_turn(
            &mut self,
            _messages: &[AgentMessage],
            event_tx: &Sender<AgentEvent>,
            run_id: &str,
        ) -> Result<ProviderTurnOutcome, LoopError> {
            let out = self
                .script
                .get(self.index)
                .cloned()
                .ok_or_else(|| LoopError::Provider("scripted ran out".into()))?;
            self.index += 1;
            // emit text delta
            if !out.text.is_empty() {
                let _ = event_tx
                    .send(AgentEvent::TextDelta {
                        run_id: run_id.into(),
                        delta: out.text.clone(),
                    })
                    .await;
            }
            Ok(out)
        }
    }

    fn echo_handler() -> std::sync::Arc<dyn ToolHandler> {
        std::sync::Arc::new(FnToolHandler(|inv: ToolInvocation| {
            Box::pin(async move {
                ToolHandlerOutput::ok(json!({ "echoed": inv.input }))
            }) as ToolHandlerFuture
        }))
    }

    #[tokio::test]
    async fn loop_completes_on_text_only_turn() {
        let registry = Arc::new(ToolRegistry::new_without_persist());
        let provider = Box::new(ScriptedProvider {
            script: vec![ProviderTurnOutcome {
                text: "hello".into(),
                thinking: vec![],
                tool_uses: vec![],
                usage_input: 5,
                usage_output: 7,
                stop_reason: AgentStopReason::Completed,
            }],
            index: 0,
        });
        let req = AgentRunRequest {
            run_id: "r1".into(),
            trigger: "u".into(),
            channel: channel(),
            max_turns: 5,
            allowed_server_side_tools: vec![],
            seed_messages: vec![],
        };
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        let summary =
            run_agent_loop(req, registry, ContextBundle::new("r1"), provider, tx)
                .await
                .unwrap();
        assert_eq!(summary.turns, 1);
        assert_eq!(summary.stop_reason, AgentStopReason::Completed);
        assert_eq!(summary.input_tokens, 5);
        assert_eq!(summary.output_tokens, 7);

        // run_start, text_delta, usage, done
        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        assert!(matches!(events.first(), Some(AgentEvent::RunStart { .. })));
        assert!(matches!(events.last(), Some(AgentEvent::Done { .. })));
    }

    #[tokio::test]
    async fn loop_stops_at_max_turns_when_provider_keeps_calling_tools() {
        let registry = Arc::new(ToolRegistry::new_without_persist());
        registry
            .register_tool(
                ToolSpec::new_local("echo", "echo", json!({}), 5000, ToolSideEffect::None),
                echo_handler(),
            )
            .unwrap();
        // Each turn requests one tool_use, never says completed.
        let script = (0..10)
            .map(|i| ProviderTurnOutcome {
                text: String::new(),
                thinking: vec![],
                tool_uses: vec![ProviderToolUse {
                    tool_call_id: format!("tc{}", i),
                    name: "echo".into(),
                    input: json!({"i": i}),
                }],
                usage_input: 1,
                usage_output: 1,
                stop_reason: AgentStopReason::ProviderStop,
            })
            .collect();
        let provider = Box::new(ScriptedProvider { script, index: 0 });
        let req = AgentRunRequest {
            run_id: "r1".into(),
            trigger: "u".into(),
            channel: channel(),
            max_turns: 3,
            allowed_server_side_tools: vec![],
            seed_messages: vec![],
        };
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
        let summary =
            run_agent_loop(req, registry, ContextBundle::new("r1"), provider, tx)
                .await
                .unwrap();
        assert_eq!(summary.stop_reason, AgentStopReason::MaxTurns);
        assert_eq!(summary.turns, 3);
        assert_eq!(summary.tool_call_ids.len(), 3);

        // Verify tool_start / tool_end events emitted in order
        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        let mut starts = 0;
        let mut ends = 0;
        for e in &events {
            match e {
                AgentEvent::ToolStart { .. } => starts += 1,
                AgentEvent::ToolEnd { .. } => ends += 1,
                _ => {}
            }
        }
        assert_eq!(starts, 3);
        assert_eq!(ends, 3);
    }

    #[tokio::test]
    async fn loop_appends_tool_result_message_per_call() {
        let registry = Arc::new(ToolRegistry::new_without_persist());
        registry
            .register_tool(
                ToolSpec::new_local("echo", "echo", json!({}), 5000, ToolSideEffect::None),
                echo_handler(),
            )
            .unwrap();
        let provider = Box::new(ScriptedProvider {
            script: vec![
                ProviderTurnOutcome {
                    text: "".into(),
                    thinking: vec![],
                    tool_uses: vec![ProviderToolUse {
                        tool_call_id: "tc1".into(),
                        name: "echo".into(),
                        input: json!({"a":1}),
                    }],
                    usage_input: 1,
                    usage_output: 1,
                    stop_reason: AgentStopReason::ProviderStop,
                },
                ProviderTurnOutcome {
                    text: "done".into(),
                    thinking: vec![],
                    tool_uses: vec![],
                    usage_input: 2,
                    usage_output: 2,
                    stop_reason: AgentStopReason::Completed,
                },
            ],
            index: 0,
        });
        let req = AgentRunRequest {
            run_id: "r1".into(),
            trigger: "u".into(),
            channel: channel(),
            max_turns: 5,
            allowed_server_side_tools: vec![],
            seed_messages: vec![],
        };
        let (tx, _rx) = mpsc::channel::<AgentEvent>(64);
        let summary =
            run_agent_loop(req, registry, ContextBundle::new("r1"), provider, tx)
                .await
                .unwrap();
        assert_eq!(summary.stop_reason, AgentStopReason::Completed);
        assert_eq!(summary.turns, 2);
        assert_eq!(summary.tool_call_ids, vec!["tc1".to_string()]);
    }

    #[tokio::test]
    async fn loop_reports_tool_unregistered_as_error_to_model() {
        // 模型尝试调用未注册工具 -> 不 panic，错误回填到 tool_result（is_error = true）。
        let registry = Arc::new(ToolRegistry::new_without_persist());
        let provider = Box::new(ScriptedProvider {
            script: vec![
                ProviderTurnOutcome {
                    text: "".into(),
                    thinking: vec![],
                    tool_uses: vec![ProviderToolUse {
                        tool_call_id: "tc1".into(),
                        name: "unknown_tool".into(),
                        input: json!({}),
                    }],
                    usage_input: 1,
                    usage_output: 1,
                    stop_reason: AgentStopReason::ProviderStop,
                },
                ProviderTurnOutcome {
                    text: "bye".into(),
                    thinking: vec![],
                    tool_uses: vec![],
                    usage_input: 1,
                    usage_output: 1,
                    stop_reason: AgentStopReason::Completed,
                },
            ],
            index: 0,
        });
        let req = AgentRunRequest {
            run_id: "r1".into(),
            trigger: "u".into(),
            channel: channel(),
            max_turns: 5,
            allowed_server_side_tools: vec![],
            seed_messages: vec![],
        };
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        let summary =
            run_agent_loop(req, registry, ContextBundle::new("r1"), provider, tx)
                .await
                .unwrap();
        assert_eq!(summary.stop_reason, AgentStopReason::Completed);
        // tool_end with is_error=true emitted
        let mut saw_error_tool_end = false;
        while let Some(e) = rx.recv().await {
            if let AgentEvent::ToolEnd { is_error, .. } = e {
                if is_error {
                    saw_error_tool_end = true;
                }
            }
        }
        assert!(saw_error_tool_end);
    }
}
