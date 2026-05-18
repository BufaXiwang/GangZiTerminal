//! Agent Loop——所有 pipeline 共用的 tool-use 迭代器。
//!
//! 单条 run 的形态：
//!   1. 调 provider.stream() 拿当回合的 assistant 消息
//!   2. 把 assistant 消息追加到 messages
//!   3. 找出消息里需要本地执行的 ToolUse（server_side=false 的）
//!   4. 并行调度 ToolRegistry 执行（每个 tool 各自带超时），结果作为 user 消息再追加
//!   5. 没有需要本地执行的 ToolUse → 结束（emit Done）；否则 ContextManager 压缩 → 回到 1

use crate::pipeline::agent::tools::{Tool, ToolContext, ToolRegistry};
use crate::domain::agent::types::{
    AgentEvent, AgentRequest, Block, CompactTier, Message, Role, StopReason, ToolResultContent,
};
use crate::infrastructure::agent::provider::{ChatProvider, ProviderError, ProviderEvent};
use crate::pipeline::agent::compact::summarize_messages;
use crate::pipeline::agent::context::{compact_if_needed, estimate_tokens, CompactAction};
use futures_util::stream::{FuturesUnordered, StreamExt};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::mpsc::UnboundedSender;

const DEFAULT_TOOL_TIMEOUT_SECS: u32 = 30;

/// Summarize tier 的运行配置。chat pipeline 传 Some；briefing/review 传 None
/// （那两个 pipeline 输入是 bounded 的，不需要 LLM 摘要）。
///
/// `provider` 默认 None——表示用 loop 的主 provider（chat 用哪个就用哪个）跑摘要。
/// 给 Some 时表示**摘要走另一个渠道**（典型场景：chat 用 opus，compact 用 haiku，
/// 但两条用的是不同 base_url + token，不能复用同一个 reqwest client）。
#[derive(Clone)]
pub struct SummarizeOptions {
    /// 摘要专用 provider；None = 用 loop 的主 provider。
    pub provider: Option<Arc<dyn ChatProvider>>,
    /// 摘要专用模型 id（如 `claude-haiku-4-5` / `gpt-5.5-nano`）。
    pub model: String,
    /// MicroClear 之后若仍超这个 token 数，触发 Summarize。
    pub trigger_threshold_tokens: u32,
    /// 单 run 内 Summarize 连续失败到这个数后熔断——本次 run 不再尝试。
    pub max_consecutive_failures: u32,
    /// Summarize 后尾部保留的 user/assistant 对数（一对 = 2 条消息）。
    pub keep_last_n_turns: u32,
}

/// 一次 run 的统计——pipeline 拿来落 agent_runs 表。
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub run_id: String,
    pub turns: u32,
    pub stop_reason: StopReason,
    pub final_message: Option<Message>,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub total_cache_read_tokens: u32,
    pub total_cache_write_tokens: u32,
    pub local_tool_calls: u32,
    pub server_tool_calls: u32,
    /// Summarize tier 实际触发并成功时的边界文本——pipeline 拿这个落
    /// chat_messages 的 compact_boundary 行，让下次 chat 继承摘要。
    /// 触发多次时只保留最后一次（一次 run 内最新的边界）。
    pub last_summary_text: Option<String>,
    /// Summarize tier 触发并成功的次数。
    pub summarize_count: u32,
    /// Summarize tier 替换掉的总消息数（dropped_messages 的累加）。
    pub summarize_dropped_messages: u32,
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("loop terminated unexpectedly: {0}")]
    Protocol(String),
}

/// 跑一次 agent。事件实时通过 `event_tx` 推出去（pipeline 端把它桥到 Tauri emit
/// + observer 落表）。返回值是结构化总结，便于直接写库。
///
/// `summarize`：可选的 Summarize tier 配置——chat pipeline 传 Some 启用 LLM 摘要
/// 兜底；briefing/review 传 None 走纯规则化压缩（MicroClear → Drop）。
#[tracing::instrument(
    skip(provider, summarize, registry, req, tool_ctx, event_tx),
    fields(
        run_id = %tool_ctx.run_id,
        pipeline = %req.pipeline.as_str(),
        model = %req.options.model,
    )
)]
pub async fn run_agent(
    provider: Arc<dyn ChatProvider>,
    summarize: Option<SummarizeOptions>,
    registry: Arc<ToolRegistry>,
    mut req: AgentRequest,
    tool_ctx: ToolContext,
    event_tx: UnboundedSender<AgentEvent>,
) -> Result<RunSummary, AgentError> {
    let run_id = tool_ctx.run_id.clone();
    tracing::info!("agent run start");

    // emit RunStart 让前端立即建一条空消息占位
    let _ = event_tx.send(AgentEvent::RunStart {
        run_id: run_id.clone(),
        pipeline: req.pipeline,
        model: req.options.model.clone(),
    });

    let max_turns = req.options.max_turns.max(1);
    let tool_timeout = Duration::from_secs(
        req.options
            .tool_timeout_secs
            .unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS) as u64,
    );
    let mut turn: u32 = 0;
    // Summarize tier 熔断计数——本 run 内连续失败次数。成功则归零。
    let mut summarize_consecutive_failures: u32 = 0;
    let mut summarize_burned_out: bool = false;
    let mut summary = RunSummary {
        run_id: run_id.clone(),
        turns: 0,
        stop_reason: StopReason::EndTurn,
        final_message: None,
        total_input_tokens: 0,
        total_output_tokens: 0,
        total_cache_read_tokens: 0,
        total_cache_write_tokens: 0,
        local_tool_calls: 0,
        server_tool_calls: 0,
        last_summary_text: None,
        summarize_count: 0,
        summarize_dropped_messages: 0,
    };

    loop {
        if turn >= max_turns {
            summary.stop_reason = StopReason::MaxTurns;
            summary.turns = turn;
            tracing::warn!(turns = turn, max_turns, "agent 撞 max_turns，强制结束");
            let _ = event_tx.send(AgentEvent::Done {
                run_id: run_id.clone(),
                stop_reason: StopReason::MaxTurns,
                turns: turn,
            });
            return Ok(summary);
        }
        turn += 1;

        // 调 provider 之前先做一次 sanitize——主要给"上轮 chat history 从 DB 读出"或
        // "流中途断开导致末尾是孤儿 tool_use"兜底。
        sanitize_orphan_tool_uses(&mut req.messages);

        // 三级压缩：
        //   1) compact_if_needed (规则化)：NoOp / MicroClear / Drop / HardLimit
        //   2) Summarize（可选，仅 chat 启用）：在 MicroClear 完成后若仍超阈值，
        //      调便宜模型把老对话压成一段中文摘要 + 边界 user 消息
        //   3) 若 Summarize 失败 / 熔断 → 回退到 compact_if_needed 已经决定的结果
        //      （包括 Drop / HardLimit）
        //
        // 这样 Summarize 是 MicroClear 的"软兜底"：成功就避免老对话被整条丢，失败
        // 就回到原来的 Drop 逻辑，不会让 chat 因为远端摘要 API 抽风而挂掉。
        let messages_in = std::mem::take(&mut req.messages);
        let report = compact_if_needed(messages_in, &req.budget);
        let report_tier: Option<CompactTier> = match report.action {
            CompactAction::NoOp => None,
            CompactAction::MicroClear => Some(CompactTier::MicroClear),
            // HardLimit 表示 Drop 阶段已经丢过消息但仍超 hard——tier 上仍属 Drop，
            // 错误本身通过下面的 Err return 单独传出去。
            CompactAction::Drop | CompactAction::HardLimit => Some(CompactTier::Drop),
        };
        if let Some(tier) = report_tier {
            let summary_tokens = report
                .estimated_tokens_before
                .saturating_sub(report.estimated_tokens_after);
            let _ = event_tx.send(AgentEvent::Compacted {
                run_id: run_id.clone(),
                tier,
                dropped_messages: report.dropped_messages,
                summary_tokens,
            });
        }
        req.messages = report.messages;
        if report.action == CompactAction::HardLimit {
            // drop 完仍超 hard_limit——再调 provider 一定会被 4xx 拒，直接报错
            return Err(AgentError::Protocol(format!(
                "上下文 ~{} tokens 超过 hard_limit_tokens={}，已尽力压缩仍无法继续",
                report.estimated_tokens_after, req.budget.hard_limit_tokens
            )));
        }

        // ===== Summarize tier =====
        // 触发条件（全部满足）：
        //   - chat pipeline 提供了 SummarizeOptions
        //   - 本 run 还没熔断
        //   - MicroClear 之后估算 tokens 仍 > trigger_threshold
        if let Some(sum_cfg) = summarize.as_ref() {
            if !summarize_burned_out {
                let est = estimate_tokens(&req.messages);
                if est > sum_cfg.trigger_threshold_tokens {
                    let messages_for_summary = std::mem::take(&mut req.messages);
                    // 摘要 provider：sum_cfg 给了就用专用渠道；没给复用主 provider
                    let summary_provider = sum_cfg.provider.as_ref().unwrap_or(&provider);
                    match summarize_messages(
                        summary_provider,
                        &sum_cfg.model,
                        &messages_for_summary,
                        sum_cfg.keep_last_n_turns,
                        &req.budget,
                    )
                    .await
                    {
                        Ok(outcome) => {
                            tracing::info!(
                                run_id = %run_id,
                                input_tokens = outcome.input_tokens,
                                output_tokens = outcome.output_tokens,
                                dropped = outcome.dropped_messages,
                                "Summarize tier 成功"
                            );
                            // 累计观测
                            summary.total_input_tokens += outcome.input_tokens;
                            summary.total_output_tokens += outcome.output_tokens;
                            summary.summarize_count += 1;
                            summary.summarize_dropped_messages += outcome.dropped_messages;
                            if !outcome.boundary_summary_text.is_empty() {
                                summary.last_summary_text =
                                    Some(outcome.boundary_summary_text.clone());
                            }
                            // 替换 messages
                            req.messages = outcome.messages;
                            summarize_consecutive_failures = 0;
                            // emit 一条额外的 Compacted(Summarize) 事件让前端展示
                            // "summary 替换了 N 条早期消息"——前端按 tier 区分渲染。
                            let saved_tokens = est.saturating_sub(estimate_tokens(&req.messages));
                            let _ = event_tx.send(AgentEvent::Compacted {
                                run_id: run_id.clone(),
                                tier: CompactTier::Summarize,
                                dropped_messages: outcome.dropped_messages,
                                summary_tokens: saved_tokens,
                            });
                        }
                        Err(err) => {
                            tracing::warn!(
                                run_id = %run_id,
                                error = %err,
                                consecutive_failures = summarize_consecutive_failures + 1,
                                "Summarize tier 失败——回退到当轮原 messages，本轮按原 compact 结果继续"
                            );
                            // 失败：把 messages 还回去，按原 compact_if_needed 结果继续
                            req.messages = messages_for_summary;
                            summarize_consecutive_failures += 1;
                            if summarize_consecutive_failures >= sum_cfg.max_consecutive_failures {
                                summarize_burned_out = true;
                                tracing::warn!(
                                    run_id = %run_id,
                                    "Summarize tier 熔断——本 run 不再尝试摘要"
                                );
                            }
                            // 不重 raise——SummarizeError::Provider 也只让本次摘要失败，
                            // 主 chat 流程继续。原对话还在 messages_for_summary 里，
                            // 至少 micro_clear 已经清过老 tool result，可能仍能撑过这轮。
                            let _ = err;
                        }
                    }
                }
            }
        }

        // **关键**：compact 的 drop 阶段按单条 message 删除会撕裂 assistant.tool_use 与
        // 其后 user.tool_result 的配对——留下孤儿 tool_use 或孤儿 tool_result，
        // provider 拒收。compact 之后再跑一次双向 sanitize 修补这类撕裂。
        if matches!(
            report.action,
            CompactAction::Drop | CompactAction::HardLimit
        ) {
            sanitize_orphan_tool_uses(&mut req.messages);
        }

        let outcome = run_one_turn(&provider, &run_id, &req, &event_tx, &mut summary).await?;
        let TurnOutcome {
            assistant_message,
            stop_reason,
        } = outcome;

        // 把 assistant 消息追加到历史
        req.messages.push(assistant_message.clone());
        summary.final_message = Some(assistant_message.clone());

        // 找出需要本地执行的 ToolUse
        let local_tool_uses = collect_local_tool_uses(&assistant_message);
        // 顺带统计 server-side 工具调用次数（每个 server_side ToolUse 算一次）
        let server_tool_calls_this_turn = assistant_message
            .content
            .iter()
            .filter(|b| {
                matches!(
                    b,
                    Block::ToolUse {
                        server_side: true,
                        ..
                    }
                )
            })
            .count() as u32;
        summary.server_tool_calls += server_tool_calls_this_turn;

        if local_tool_uses.is_empty() {
            // 没有要本地执行的工具 → 模型回合自然结束。
            // MaxTokens 也走这里（模型说话说一半被截断、没出 tool_use）——pipeline 拿
            // RunSummary.stop_reason 自己决定怎么处理（chat 兜底文案，briefing/review
            // 报错 "JSON 被截断"）。loop 不在这里 panic 或 retry。
            summary.stop_reason = stop_reason;
            summary.turns = turn;
            tracing::info!(
                turns = turn,
                stop_reason = ?stop_reason,
                input_tokens = summary.total_input_tokens,
                output_tokens = summary.total_output_tokens,
                cache_read = summary.total_cache_read_tokens,
                local_tool_calls = summary.local_tool_calls,
                server_tool_calls = summary.server_tool_calls,
                "agent run done"
            );
            let _ = event_tx.send(AgentEvent::Done {
                run_id: run_id.clone(),
                stop_reason,
                turns: turn,
            });
            return Ok(summary);
        }

        // 并行执行所有本地 ToolUse（每个带 tool_timeout）
        let results = execute_tools_parallel(
            &registry,
            &tool_ctx,
            &run_id,
            local_tool_uses,
            &event_tx,
            tool_timeout,
        )
        .await;
        summary.local_tool_calls += results.len() as u32;

        // 把工具结果打包成 user 消息塞回去
        let result_blocks: Vec<Block> = results.into_iter().map(Into::into).collect();
        req.messages.push(Message {
            role: Role::User,
            content: result_blocks,
        });
    }
}

struct TurnOutcome {
    assistant_message: Message,
    stop_reason: StopReason,
}

/// 跑一回合 provider 调用——drain 完事件流，组装出 assistant Message + stop_reason。
async fn run_one_turn(
    provider: &Arc<dyn ChatProvider>,
    run_id: &str,
    req: &AgentRequest,
    event_tx: &UnboundedSender<AgentEvent>,
    summary: &mut RunSummary,
) -> Result<TurnOutcome, AgentError> {
    let mut stream = provider.stream(req).await?;
    let mut assistant: Option<Message> = None;
    let mut stop_reason = StopReason::EndTurn;

    while let Some(event) = stream.next().await {
        match event? {
            ProviderEvent::TextDelta(delta) => {
                let _ = event_tx.send(AgentEvent::TextDelta {
                    run_id: run_id.into(),
                    delta,
                });
            }
            ProviderEvent::ThinkingDelta(delta) => {
                let _ = event_tx.send(AgentEvent::Thinking {
                    run_id: run_id.into(),
                    delta,
                });
            }
            ProviderEvent::Usage(u) => {
                summary.total_input_tokens += u.input_tokens;
                summary.total_output_tokens += u.output_tokens;
                summary.total_cache_read_tokens += u.cache_read_tokens;
                summary.total_cache_write_tokens += u.cache_write_tokens;
                let _ = event_tx.send(AgentEvent::Usage {
                    run_id: run_id.into(),
                    input_tokens: u.input_tokens,
                    output_tokens: u.output_tokens,
                    cache_read_tokens: u.cache_read_tokens,
                    cache_write_tokens: u.cache_write_tokens,
                });
            }
            ProviderEvent::MessageComplete {
                message,
                stop_reason: sr,
            } => {
                assistant = Some(message);
                stop_reason = sr;
            }
        }
    }

    let assistant_message = assistant
        .ok_or_else(|| AgentError::Protocol("provider stream 结束但没有 MessageComplete".into()))?;
    Ok(TurnOutcome {
        assistant_message,
        stop_reason,
    })
}

/// 从 assistant 消息里挑出所有需要本地执行的 ToolUse。
/// server_side=true 的 ToolUse 由 provider 执行，loop 不管。
fn collect_local_tool_uses(message: &Message) -> Vec<LocalToolUse> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            Block::ToolUse {
                id,
                name,
                input,
                server_side: false,
            } => Some(LocalToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            }),
            _ => None,
        })
        .collect()
}

#[derive(Debug, Clone)]
struct LocalToolUse {
    id: String,
    name: String,
    input: serde_json::Value,
}

/// 一次工具调用的结果——包含 tool_use_id 让 provider 能匹配。
struct LocalToolResult {
    tool_use_id: String,
    content: Vec<ToolResultContent>,
    is_error: bool,
}

impl From<LocalToolResult> for Block {
    fn from(r: LocalToolResult) -> Self {
        Block::ToolResult {
            tool_use_id: r.tool_use_id,
            content: r.content,
            is_error: r.is_error,
            server_side: false,
            cache_control: false,
        }
    }
}

/// 并行执行一批 ToolUse。每个 tool 各自 emit ToolStart/ToolEnd 事件——
/// 不同 tool 的事件可能交错，前端按 tool_use_id 索引渲染即可。
async fn execute_tools_parallel(
    registry: &Arc<ToolRegistry>,
    ctx: &ToolContext,
    run_id: &str,
    tool_uses: Vec<LocalToolUse>,
    event_tx: &UnboundedSender<AgentEvent>,
    tool_timeout: Duration,
) -> Vec<LocalToolResult> {
    // 关键：保留模型给的 tool_use 顺序——OpenAI Responses 协议虽然按 call_id 匹配，
    // 但 Anthropic 对 messages 里 tool_result 的顺序在某些边界情况会更敏感，且按
    // 顺序回填便于 debug 回放。FuturesUnordered 会按完成顺序 yield，我们在每个
    // future 里带上原始 index，最后按 index 排序。
    let mut futures = FuturesUnordered::new();
    for (idx, tu) in tool_uses.into_iter().enumerate() {
        let registry = registry.clone();
        let ctx = ctx.clone();
        let event_tx = event_tx.clone();
        let run_id = run_id.to_string();
        futures.push(async move {
            let r =
                execute_single_tool(&registry, &ctx, &run_id, tu, &event_tx, tool_timeout).await;
            (idx, r)
        });
    }
    let mut indexed: Vec<(usize, LocalToolResult)> = Vec::new();
    while let Some(pair) = futures.next().await {
        indexed.push(pair);
    }
    indexed.sort_by_key(|(i, _)| *i);
    indexed.into_iter().map(|(_, r)| r).collect()
}

async fn execute_single_tool(
    registry: &ToolRegistry,
    ctx: &ToolContext,
    run_id: &str,
    tu: LocalToolUse,
    event_tx: &UnboundedSender<AgentEvent>,
    tool_timeout: Duration,
) -> LocalToolResult {
    let LocalToolUse { id, name, input } = tu;
    let _ = event_tx.send(AgentEvent::ToolStart {
        run_id: run_id.into(),
        tool_use_id: id.clone(),
        name: name.clone(),
        input: input.clone(),
        server_side: false,
    });
    let start = Instant::now();
    let (content, is_error) = match registry.get(&name) {
        Some(tool) => {
            let tool: Arc<dyn Tool> = tool.clone();
            // 用 tokio::time::timeout 给 execute 套一层；超时返回 is_error=true，
            // agent 下一轮看到 "tool 超时" 文本可以决定是否换工具或放弃。
            match tokio::time::timeout(tool_timeout, tool.execute(input.clone(), ctx)).await {
                Ok(out) => out,
                Err(_) => (
                    vec![ToolResultContent::Text {
                        text: format!(
                            "工具 {name} 调用超时（>{}s），可能远端服务卡顿。建议下一轮换个思路。",
                            tool_timeout.as_secs()
                        ),
                    }],
                    true,
                ),
            }
        }
        None => (
            vec![ToolResultContent::Text {
                text: format!("未知工具：{name}"),
            }],
            true,
        ),
    };
    let duration_ms = start.elapsed().as_millis() as u64;
    let _ = event_tx.send(AgentEvent::ToolEnd {
        run_id: run_id.into(),
        tool_use_id: id.clone(),
        name,
        output: content.clone(),
        is_error,
        duration_ms,
        server_side: false,
    });
    LocalToolResult {
        tool_use_id: id,
        content,
        is_error,
    }
}

/// 扫一遍 messages，做双向 tool_use ↔ tool_result 配对修复：
/// 1. 缺失 tool_result：补 stub error 进末尾 user 消息（或新建一条）
/// 2. 孤儿 tool_result（tool_use_id 没有任何对应的 tool_use）：直接删除该 block
///
/// 这层防御对 4 种场景都生效：
/// - 流中断（assistant message 含 tool_use 但 result 丢失）→ 缺 result 补 stub
/// - chat resume（DB 读出的 history 末尾恰好是孤儿 tool_use）→ 同上
/// - **compact 撕裂**（drop 阶段移除最老的 assistant message 留下孤儿
///   tool_result；或反之留下孤儿 tool_use）→ 补 stub + 删 orphan result
/// - 序列化往返中 block 顺序异常 → 同上
///
/// 参考 claude code `ensureToolResultPairing`（utils/messages.ts:5133）。
fn sanitize_orphan_tool_uses(messages: &mut Vec<Message>) {
    // 第一遍：枚举所有 tool_use_id（local only——server_side 由 provider 自己回 result）
    // 和所有出现过的 tool_result.tool_use_id。
    let mut tool_use_ids: Vec<String> = Vec::new();
    let mut result_target_ids: HashSet<String> = HashSet::new();
    for msg in messages.iter() {
        for block in &msg.content {
            match block {
                Block::ToolUse {
                    id, server_side, ..
                } => {
                    if !*server_side {
                        tool_use_ids.push(id.clone());
                    }
                }
                Block::ToolResult { tool_use_id, .. } => {
                    result_target_ids.insert(tool_use_id.clone());
                }
                _ => {}
            }
        }
    }
    let tool_use_id_set: HashSet<String> = tool_use_ids.iter().cloned().collect();

    // (a) 缺失 tool_result 的 tool_use
    let missing_results: Vec<String> = tool_use_ids
        .iter()
        .filter(|id| !result_target_ids.contains(*id))
        .cloned()
        .collect();
    // (b) 孤儿 tool_result（其 tool_use_id 不在任何 tool_use 中）
    let mut had_orphan_result = false;

    // 先做 (b)：原地删孤儿 tool_result block。
    // 删完后若某 user 消息变空，把整条消息也删掉（provider 拒收空 content 数组）。
    messages.retain_mut(|msg| {
        msg.content.retain(|block| match block {
            Block::ToolResult { tool_use_id, .. } if !tool_use_id_set.contains(tool_use_id) => {
                had_orphan_result = true;
                false
            }
            _ => true,
        });
        !msg.content.is_empty()
    });
    if had_orphan_result {
        tracing::warn!("移除了孤儿 tool_result（无对应 tool_use）——多见于 compaction drop 后");
    }

    // 再做 (a)：给缺失 tool_result 的 tool_use 补 stub
    if missing_results.is_empty() {
        return;
    }
    tracing::warn!(
        count = missing_results.len(),
        "为孤儿 tool_use 补 stub tool_result"
    );
    let stubs: Vec<Block> = missing_results
        .into_iter()
        .map(|id| Block::ToolResult {
            tool_use_id: id,
            content: vec![ToolResultContent::Text {
                text: "[interrupted: 工具调用未完成，结果丢失]".into(),
            }],
            is_error: true,
            server_side: false,
            cache_control: false,
        })
        .collect();
    if let Some(last) = messages.last_mut() {
        if matches!(last.role, Role::User) {
            last.content.extend(stubs);
            return;
        }
    }
    messages.push(Message {
        role: Role::User,
        content: stubs,
    });
}

// ===== 单元测试 ==========================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sanitize_inserts_stub_for_orphan_tool_use() {
        let mut messages = vec![
            Message {
                role: Role::User,
                content: vec![Block::Text {
                    text: "查行情".into(),
                    cache_control: false,
                }],
            },
            // 上轮 assistant 调了工具但 result 丢了（流中断模拟）
            Message {
                role: Role::Assistant,
                content: vec![Block::ToolUse {
                    id: "toolu_orphan".into(),
                    name: "get_quote".into(),
                    input: json!({}),
                    server_side: false,
                }],
            },
            // user 接着发新问题——本来该有 tool_result 的位置缺了
            Message {
                role: Role::User,
                content: vec![Block::Text {
                    text: "再问一句".into(),
                    cache_control: false,
                }],
            },
        ];
        sanitize_orphan_tool_uses(&mut messages);
        // 末尾 user 消息应该被补入 stub tool_result
        let last = messages.last().unwrap();
        let has_stub = last.content.iter().any(|b| {
            matches!(
                b,
                Block::ToolResult {
                    tool_use_id, is_error, ..
                } if tool_use_id == "toolu_orphan" && *is_error
            )
        });
        assert!(has_stub, "孤儿 tool_use 应该被补 stub tool_result");
    }

    #[test]
    fn sanitize_appends_user_msg_when_last_is_assistant() {
        let mut messages = vec![Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "toolu_orphan".into(),
                name: "get_quote".into(),
                input: json!({}),
                server_side: false,
            }],
        }];
        sanitize_orphan_tool_uses(&mut messages);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].role, Role::User);
    }

    #[test]
    fn sanitize_noop_when_all_paired() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![Block::ToolUse {
                    id: "toolu_1".into(),
                    name: "x".into(),
                    input: json!({}),
                    server_side: false,
                }],
            },
            Message {
                role: Role::User,
                content: vec![Block::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: vec![ToolResultContent::Text { text: "ok".into() }],
                    is_error: false,
                    server_side: false,
                    cache_control: false,
                }],
            },
        ];
        let before = messages.len();
        sanitize_orphan_tool_uses(&mut messages);
        assert_eq!(messages.len(), before, "已配对时不应改变 messages");
    }

    #[test]
    fn sanitize_strips_orphan_tool_results() {
        // compaction drop 阶段移除最老的 assistant 留下孤儿 user.tool_result——
        // 应该被原地删除（保留这块的话 provider 会因 unmatched tool_use_id 拒收）
        let mut messages = vec![
            // 假设这是 compact 后只剩的 user 消息
            Message {
                role: Role::User,
                content: vec![Block::ToolResult {
                    tool_use_id: "toolu_dropped".into(),
                    content: vec![ToolResultContent::Text { text: "old".into() }],
                    is_error: false,
                    server_side: false,
                    cache_control: false,
                }],
            },
            Message {
                role: Role::User,
                content: vec![Block::Text {
                    text: "新问题".into(),
                    cache_control: false,
                }],
            },
        ];
        sanitize_orphan_tool_uses(&mut messages);
        // 第一条整个被删（删完 content 为空 → 整条删）
        assert_eq!(messages.len(), 1);
        match &messages[0].content[0] {
            Block::Text { text, .. } => assert_eq!(text, "新问题"),
            _ => panic!(),
        }
    }

    #[test]
    fn sanitize_handles_compact_torn_pair_both_directions() {
        // 输入模拟 compact drop 撕裂的状态：第 0 条原本配对的 assistant 已被丢，
        // 留下孤儿 tool_result；同时末尾还有个新 assistant.tool_use 没 result 的。
        let mut messages = vec![
            Message {
                role: Role::User,
                content: vec![
                    Block::Text {
                        text: "[上下文压缩：N 条更早的消息已省略]".into(),
                        cache_control: false,
                    },
                    Block::ToolResult {
                        tool_use_id: "toolu_orphan_old".into(),
                        content: vec![ToolResultContent::Text {
                            text: "stale".into(),
                        }],
                        is_error: false,
                        server_side: false,
                        cache_control: false,
                    },
                ],
            },
            Message {
                role: Role::Assistant,
                content: vec![Block::ToolUse {
                    id: "toolu_unmatched".into(),
                    name: "get_quote".into(),
                    input: json!({}),
                    server_side: false,
                }],
            },
        ];
        sanitize_orphan_tool_uses(&mut messages);
        // (a) 孤儿 tool_result 被删——第一条仍存在但只剩 Text
        assert_eq!(messages[0].content.len(), 1);
        assert!(matches!(messages[0].content[0], Block::Text { .. }));
        // (b) 末尾新插一条 user 含 stub tool_result for toolu_unmatched
        let last = messages.last().unwrap();
        assert_eq!(last.role, Role::User);
        let has_stub = last.content.iter().any(|b| matches!(b, Block::ToolResult { tool_use_id, .. } if tool_use_id == "toolu_unmatched"));
        assert!(has_stub);
    }

    #[test]
    fn sanitize_skips_server_side_tool_uses() {
        // server_side=true 的 ToolUse 由 provider 自己回 result，不该被当孤儿补
        let mut messages = vec![Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "srvtoolu_1".into(),
                name: "web_search".into(),
                input: json!({}),
                server_side: true,
            }],
        }];
        let before = messages.len();
        sanitize_orphan_tool_uses(&mut messages);
        assert_eq!(messages.len(), before);
    }

    #[test]
    fn collect_local_tool_uses_skips_server_side() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                Block::Text {
                    text: "好".into(),
                    cache_control: false,
                },
                Block::ToolUse {
                    id: "toolu_1".into(),
                    name: "get_quote".into(),
                    input: json!({"code": "600519"}),
                    server_side: false,
                },
                Block::ToolUse {
                    id: "srvtoolu_1".into(),
                    name: "web_search".into(),
                    input: json!({"query": "茅台"}),
                    server_side: true,
                },
            ],
        };
        let uses = collect_local_tool_uses(&msg);
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].name, "get_quote");
    }

    #[test]
    fn local_tool_result_to_block_preserves_id_and_error() {
        let r = LocalToolResult {
            tool_use_id: "toolu_42".into(),
            content: vec![ToolResultContent::Text {
                text: "error".into(),
            }],
            is_error: true,
        };
        let block: Block = r.into();
        match block {
            Block::ToolResult {
                tool_use_id,
                is_error,
                server_side,
                ..
            } => {
                assert_eq!(tool_use_id, "toolu_42");
                assert!(is_error);
                assert!(!server_side);
            }
            _ => panic!("expected ToolResult"),
        }
    }

    // ===== 端到端：FakeProvider + FakeTool 走完整 loop =====

    use crate::pipeline::agent::tools::Tool;
    use crate::domain::agent::types::{
        AgentOptions, ContextBudget, PipelineKind, SystemBlock, ToolDef,
    };
    use crate::infrastructure::agent::provider::{ChatProvider, ProviderEvent, TokenUsage};
    use async_trait::async_trait;
    use futures_util::stream::{self, BoxStream};
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    /// 一个脚本化的 fake provider——按 turn 顺序返回预制的回合。
    struct FakeProvider {
        scripted_turns: Mutex<Vec<Vec<ProviderEvent>>>,
    }
    impl FakeProvider {
        fn new(turns: Vec<Vec<ProviderEvent>>) -> Self {
            Self {
                scripted_turns: Mutex::new(turns),
            }
        }
    }
    #[async_trait]
    impl ChatProvider for FakeProvider {
        async fn stream(
            &self,
            _req: &AgentRequest,
        ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError>
        {
            let mut guard = self.scripted_turns.lock().unwrap();
            if guard.is_empty() {
                return Err(ProviderError::Protocol(
                    "fake provider 脚本耗尽——loop 跑了比预期更多的回合".into(),
                ));
            }
            let events = guard.remove(0);
            let stream = stream::iter(events.into_iter().map(Ok));
            Ok(stream.boxed())
        }
    }

    /// 一个 fake tool——记录被调用次数和最后的 input。
    struct FakeTool {
        name: &'static str,
        calls: Arc<Mutex<u32>>,
        last_input: Arc<Mutex<Option<serde_json::Value>>>,
        response: serde_json::Value,
    }
    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            "fake"
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn execute(
            &self,
            input: serde_json::Value,
            _ctx: &ToolContext,
        ) -> (Vec<ToolResultContent>, bool) {
            *self.calls.lock().unwrap() += 1;
            *self.last_input.lock().unwrap() = Some(input);
            (
                vec![ToolResultContent::Text {
                    text: self.response.to_string(),
                }],
                false,
            )
        }
    }

    fn dummy_request(messages: Vec<Message>) -> AgentRequest {
        AgentRequest {
            system: vec![SystemBlock {
                text: "你是测试 agent".into(),
                cache_control: false,
            }],
            tools: vec![ToolDef::Local {
                name: "get_quote".into(),
                description: "fake".into(),
                input_schema: json!({"type": "object"}),
                cache_control: false,
            }],
            messages,
            options: AgentOptions {
                model: "fake-model".into(),
                max_tokens: 1024,
                temperature: None,
                top_p: None,
                thinking: None,
                effort: None,
                max_turns: 5,
                stop_sequences: vec![],
                tool_timeout_secs: None,
            },
            budget: ContextBudget {
                soft_limit_tokens: 100_000,
                hard_limit_tokens: 200_000,
                compact_keep_last_n: 6,
                max_search_calls: 5,
            },
            trigger_message_id: None,
            pipeline: PipelineKind::Chat,
        }
    }

    fn drain_events(rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        events
    }

    #[tokio::test]
    async fn loop_runs_one_turn_when_no_tool_use() {
        // turn 1: 直接返回最终文本，无 tool_use
        let provider: Arc<dyn ChatProvider> = Arc::new(FakeProvider::new(vec![vec![
            ProviderEvent::TextDelta("你".into()),
            ProviderEvent::TextDelta("好".into()),
            ProviderEvent::Usage(TokenUsage {
                input_tokens: 10,
                output_tokens: 2,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
            ProviderEvent::MessageComplete {
                message: Message {
                    role: Role::Assistant,
                    content: vec![Block::Text {
                        text: "你好".into(),
                        cache_control: false,
                    }],
                },
                stop_reason: StopReason::EndTurn,
            },
        ]]));
        let registry = Arc::new(ToolRegistry::new());
        let req = dummy_request(vec![Message {
            role: Role::User,
            content: vec![Block::Text {
                text: "嗨".into(),
                cache_control: false,
            }],
        }]);
        let ctx = ToolContext {
            run_id: "run-1".into(),
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let summary = run_agent(provider, None, registry, req, ctx, tx)
            .await
            .unwrap();
        assert_eq!(summary.turns, 1);
        assert_eq!(summary.stop_reason, StopReason::EndTurn);
        assert_eq!(summary.local_tool_calls, 0);
        assert_eq!(summary.total_input_tokens, 10);
        assert_eq!(summary.total_output_tokens, 2);

        let events = drain_events(&mut rx);
        // RunStart, TextDelta×2, Usage, Done
        assert!(matches!(events[0], AgentEvent::RunStart { .. }));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::TextDelta { .. }))
                .count(),
            2
        );
        assert!(matches!(events.last(), Some(AgentEvent::Done { .. })));
    }

    #[tokio::test]
    async fn loop_executes_tool_use_then_continues() {
        let calls = Arc::new(Mutex::new(0u32));
        let last_input = Arc::new(Mutex::new(None));
        let fake_tool = Arc::new(FakeTool {
            name: "get_quote",
            calls: calls.clone(),
            last_input: last_input.clone(),
            response: json!({"price": 1888.0}),
        });
        let mut registry = ToolRegistry::new();
        registry.register(fake_tool);
        let registry = Arc::new(registry);

        // turn 1: 模型说"我去查一下"+ tool_use
        // turn 2: 模型基于工具结果给最终答案
        let provider: Arc<dyn ChatProvider> = Arc::new(FakeProvider::new(vec![
            vec![
                ProviderEvent::TextDelta("我查一下行情。".into()),
                ProviderEvent::Usage(TokenUsage {
                    input_tokens: 50,
                    output_tokens: 20,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                }),
                ProviderEvent::MessageComplete {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![
                            Block::Text {
                                text: "我查一下行情。".into(),
                                cache_control: false,
                            },
                            Block::ToolUse {
                                id: "toolu_a".into(),
                                name: "get_quote".into(),
                                input: json!({"code": "600519"}),
                                server_side: false,
                            },
                        ],
                    },
                    stop_reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("茅台 1888 元".into()),
                ProviderEvent::Usage(TokenUsage {
                    input_tokens: 80,
                    output_tokens: 10,
                    cache_read_tokens: 50,
                    cache_write_tokens: 0,
                }),
                ProviderEvent::MessageComplete {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![Block::Text {
                            text: "茅台 1888 元".into(),
                            cache_control: false,
                        }],
                    },
                    stop_reason: StopReason::EndTurn,
                },
            ],
        ]));

        let req = dummy_request(vec![Message {
            role: Role::User,
            content: vec![Block::Text {
                text: "茅台多少钱".into(),
                cache_control: false,
            }],
        }]);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let summary = run_agent(
            provider,
            None,
            registry,
            req,
            ToolContext {
                run_id: "run-2".into(),
            },
            tx,
        )
        .await
        .unwrap();

        // 工具应该被调用恰好 1 次，input 是模型给的 code=600519
        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(
            last_input.lock().unwrap().as_ref().unwrap()["code"],
            "600519"
        );
        // 总共 2 个 turn
        assert_eq!(summary.turns, 2);
        assert_eq!(summary.local_tool_calls, 1);
        // token 累加
        assert_eq!(summary.total_input_tokens, 50 + 80);
        assert_eq!(summary.total_output_tokens, 20 + 10);
        assert_eq!(summary.total_cache_read_tokens, 50);

        let events = drain_events(&mut rx);
        // 必须有 ToolStart 和 ToolEnd 各一条
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::ToolStart { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::ToolEnd { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn loop_stops_at_max_turns() {
        // 模型每回合都给 tool_use → 永远不收敛 → 必须被 max_turns 截断
        let make_tool_use_turn = || {
            vec![
                ProviderEvent::Usage(TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                }),
                ProviderEvent::MessageComplete {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![Block::ToolUse {
                            id: format!("toolu_{}", uuid::Uuid::new_v4()),
                            name: "get_quote".into(),
                            input: json!({"code": "600519"}),
                            server_side: false,
                        }],
                    },
                    stop_reason: StopReason::EndTurn,
                },
            ]
        };
        let provider: Arc<dyn ChatProvider> = Arc::new(FakeProvider::new(vec![
            make_tool_use_turn(),
            make_tool_use_turn(),
            make_tool_use_turn(),
            // 第 4、5 回合脚本耗尽——但 max_turns=3 应该在那之前结束
        ]));
        let calls = Arc::new(Mutex::new(0u32));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(FakeTool {
            name: "get_quote",
            calls: calls.clone(),
            last_input: Arc::new(Mutex::new(None)),
            response: json!({}),
        }));
        let registry = Arc::new(registry);

        let mut req = dummy_request(vec![Message {
            role: Role::User,
            content: vec![Block::Text {
                text: "嗨".into(),
                cache_control: false,
            }],
        }]);
        req.options.max_turns = 3;

        let (tx, _rx) = mpsc::unbounded_channel();
        let summary = run_agent(
            provider,
            None,
            registry,
            req,
            ToolContext {
                run_id: "run-3".into(),
            },
            tx,
        )
        .await
        .unwrap();
        assert_eq!(summary.stop_reason, StopReason::MaxTurns);
        // 每个 turn 一次 tool 调用，3 个 turn → 3 次（loop 在第 4 个 turn 进入前判断 max_turns，
        // 前 3 个 turn 都跑完了 tool 执行）
        assert_eq!(*calls.lock().unwrap(), 3);
        // turns 字段必须等于 max_turns，不能停在 0（之前漏赋值的 bug）
        assert_eq!(summary.turns, 3);
    }

    /// 慢工具触发 tool_timeout——结果应该是 is_error=true 并带"超时"文案，
    /// loop 不应该被卡死。
    #[tokio::test]
    async fn tool_timeout_returns_is_error_without_blocking() {
        struct SlowTool;
        #[async_trait]
        impl Tool for SlowTool {
            fn name(&self) -> &'static str {
                "slow_thing"
            }
            fn description(&self) -> &'static str {
                "intentionally slow"
            }
            fn input_schema(&self) -> serde_json::Value {
                json!({"type": "object"})
            }
            async fn execute(
                &self,
                _input: serde_json::Value,
                _ctx: &ToolContext,
            ) -> (Vec<ToolResultContent>, bool) {
                // 故意睡 5s——tool_timeout 设 1s 应该截断
                tokio::time::sleep(Duration::from_secs(5)).await;
                (
                    vec![ToolResultContent::Text {
                        text: "never".into(),
                    }],
                    false,
                )
            }
        }
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(SlowTool));
        let registry = Arc::new(registry);

        // turn 1: 模型发起 slow_thing → 超时 → turn 2: 模型看到 error 收尾
        let provider: Arc<dyn ChatProvider> = Arc::new(FakeProvider::new(vec![
            vec![
                ProviderEvent::Usage(TokenUsage::default()),
                ProviderEvent::MessageComplete {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![Block::ToolUse {
                            id: "toolu_slow".into(),
                            name: "slow_thing".into(),
                            input: json!({}),
                            server_side: false,
                        }],
                    },
                    stop_reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::Usage(TokenUsage::default()),
                ProviderEvent::MessageComplete {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![Block::Text {
                            text: "工具卡了，先到这".into(),
                            cache_control: false,
                        }],
                    },
                    stop_reason: StopReason::EndTurn,
                },
            ],
        ]));

        let mut req = dummy_request(vec![Message {
            role: Role::User,
            content: vec![Block::Text {
                text: "试试".into(),
                cache_control: false,
            }],
        }]);
        req.options.tool_timeout_secs = Some(1);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let started = Instant::now();
        let summary = run_agent(
            provider,
            None,
            registry,
            req,
            ToolContext {
                run_id: "run-timeout".into(),
            },
            tx,
        )
        .await
        .unwrap();
        let elapsed = started.elapsed();
        // 1s timeout + 一点 overhead。如果超 4s 说明 timeout 没生效
        assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
        assert_eq!(summary.turns, 2);
        let events = drain_events(&mut rx);
        let timed_out_end = events.iter().any(|e| matches!(e, AgentEvent::ToolEnd { is_error, output, .. } if *is_error && output.iter().any(|c| matches!(c, ToolResultContent::Text { text } if text.contains("超时")))));
        assert!(
            timed_out_end,
            "应该 emit 一条 is_error=true 且文案含'超时'的 ToolEnd"
        );
    }

    /// content_block_start 跳号时返回 Protocol 错误（修复前是 panic）。
    /// 这个测试在 anthropic.rs::SseDecoder 的 module 里更合适，但放在 loop 测里
    /// 也能验证错误能正常上浮。
    #[tokio::test]
    async fn loop_propagates_provider_error() {
        struct ErrorProvider;
        #[async_trait]
        impl ChatProvider for ErrorProvider {
            async fn stream(
                &self,
                _req: &AgentRequest,
            ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError>
            {
                Err(ProviderError::Request {
                    status: 401,
                    body: "fake auth fail".into(),
                })
            }
        }
        let provider: Arc<dyn ChatProvider> = Arc::new(ErrorProvider);
        let registry = Arc::new(ToolRegistry::new());
        let req = dummy_request(vec![Message {
            role: Role::User,
            content: vec![Block::Text {
                text: "hi".into(),
                cache_control: false,
            }],
        }]);
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = run_agent(
            provider,
            None,
            registry,
            req,
            ToolContext {
                run_id: "run-err".into(),
            },
            tx,
        )
        .await;
        assert!(matches!(result, Err(AgentError::Provider(_))));
    }
}
