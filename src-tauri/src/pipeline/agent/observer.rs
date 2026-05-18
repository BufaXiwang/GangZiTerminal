//! 把 [`AgentEvent`] 流桥到「Tauri emit + agent_runs 表」。
//!
//! Pipeline 用法（chat 和 runner 都是这条模式）：
//! ```ignore
//! let run_id = uuid::Uuid::new_v4().to_string();
//! observer::start_run(&app, &run_id, pipeline, provider_name, model, trigger_msg_id)?;
//! let (tx, mut rx) = mpsc::unbounded_channel();
//! let collector = tokio::spawn(async move {
//!     while let Some(ev) = rx.recv().await {
//!         let _ = app.emit(observer::AGENT_EVENT, &ev);  // 转发给前端
//!         // 顺便累积所需事件（如 TextDelta 拼最终文本）
//!     }
//! });
//! let summary = run_agent(provider, registry, req, ctx, tx).await?;
//! observer::finalize(&app, &summary, None)?;
//! ```

use crate::domain::agent::types::{AgentEvent, PipelineKind, StopReason};
use crate::infrastructure::agent::repository::{
    finalize_agent_run, insert_agent_run_start, insert_agent_run_turn,
};
use crate::pipeline::agent::RunSummary;
use chrono::Utc;
use std::collections::HashMap;
use tauri::AppHandle;

/// 前端唯一接收 channel——所有 pipeline、所有 run 共享同一个 Tauri event 名。
/// 前端 listen 一次，按 payload.run_id 区分归属。
pub const AGENT_EVENT: &str = "agent-event";

pub fn start_run(
    app: &AppHandle,
    run_id: &str,
    pipeline: PipelineKind,
    provider: &str,
    model: &str,
    trigger_message_id: Option<&str>,
) -> Result<String, String> {
    let started_at = Utc::now().to_rfc3339();
    insert_agent_run_start(
        app,
        run_id,
        pipeline.as_str(),
        provider,
        model,
        &started_at,
        trigger_message_id,
    )?;
    Ok(started_at)
}

pub fn finalize(app: &AppHandle, summary: &RunSummary, error: Option<&str>) -> Result<(), String> {
    let ended_at = Utc::now().to_rfc3339();
    finalize_agent_run(
        app,
        &summary.run_id,
        &ended_at,
        summary.turns,
        summary.total_input_tokens,
        summary.total_output_tokens,
        summary.total_cache_read_tokens,
        summary.total_cache_write_tokens,
        summary.local_tool_calls,
        summary.server_tool_calls,
        Some(stop_reason_str(summary.stop_reason)),
        error,
    )
}

/// run 启动失败（连模型都没调通）——只补一条 ended_at + error，不带 token 数据。
pub fn finalize_failure(app: &AppHandle, run_id: &str, error: &str) -> Result<(), String> {
    let ended_at = Utc::now().to_rfc3339();
    finalize_agent_run(
        app,
        run_id,
        &ended_at,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        None,
        Some(error),
    )
}

/// pipeline 端的 collector 在 await run_agent 时同步累积 per-turn 状态——
/// 这里把累积状态翻译成 agent_run_turns 行写入。每条 run 会调用 N 次（N = turns 数）。
///
/// 用法：collector 在收到 [`AgentEvent::Done`] 之前，按 [`AgentEvent::Usage`] /
/// [`AgentEvent::ToolStart`] / [`AgentEvent::ToolEnd`] 切分 turn，调用本函数 flush。
/// 最简单的做法：每收到一个 Usage 视为一个 turn 的边界（Anthropic / OpenAI 都是
/// 一回合一条 Usage）。
#[allow(clippy::too_many_arguments)]
pub fn record_turn(
    app: &AppHandle,
    run_id: &str,
    turn: u32,
    started_at: &str,
    ended_at: &str,
    stop_reason: Option<StopReason>,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    local_tool_calls: u32,
    server_tool_calls: u32,
    error: Option<&str>,
) -> Result<(), String> {
    insert_agent_run_turn(
        app,
        run_id,
        turn,
        started_at,
        ended_at,
        stop_reason.map(stop_reason_str),
        input_tokens,
        output_tokens,
        cache_read_tokens,
        local_tool_calls,
        server_tool_calls,
        error,
    )
}

/// AgentEvent 流的 turn 切分器——pipeline 的 collector 调用：
/// - `consume(ev)` —— 累计单 turn 的 token + tool count
/// - 当 `consume` 返回 `Some(TurnRecord)` 时，把它持久化到 agent_run_turns
///
/// **事件顺序**（关键，决定切分逻辑）：每 turn 的 provider stream 先 emit Usage
/// 然后 MessageComplete；loop 拿到 message 之后才执行工具，因此 ToolStart/ToolEnd
/// 在 **Usage 之后** 才发出。下一回合的 Usage 之前都是当前 turn 的工具事件。
///
/// 早期实现把 Usage 当 turn 结束边界，立即 flush——结果纯 tool-only turn（assistant
/// 只调工具不说话）会因 `seen_content=false` 跳过 Usage，下一轮的 ToolStart/End
/// 错归到下一 turn。
///
/// 现在改成 **pending-usage** 模式：
/// - Usage：把 token 数暂存到 pending；如果**已经**有 pending（说明上 turn 完成），
///   先 flush 上一 turn（含其后到达的 ToolStart/End 计数）。
/// - ToolStart：累加到当前未 flush 的 turn 的工具计数。
/// - Done：如果有 pending，做最后一次 flush。
pub struct TurnAccumulator {
    run_id: String,
    current_turn: u32,
    started_at: String,
    /// 是否有等待 flush 的 turn（已收到 Usage，等下次 Usage 或 Done 触发 flush）
    has_pending: bool,
    pending_input_tokens: u32,
    pending_output_tokens: u32,
    pending_cache_read_tokens: u32,
    /// 工具计数——挂在"当前 pending turn"上。flush 时清零。
    local_tool_calls: u32,
    server_tool_calls: u32,
}

#[derive(Debug, Clone)]
pub struct TurnRecord {
    pub turn: u32,
    pub started_at: String,
    pub ended_at: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub local_tool_calls: u32,
    pub server_tool_calls: u32,
    /// 仅在 Done 事件触发时填——中间 turn 的 flush 没有 stop_reason。
    pub stop_reason: Option<StopReason>,
}

impl TurnAccumulator {
    pub fn new(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            current_turn: 1,
            started_at: Utc::now().to_rfc3339(),
            has_pending: false,
            pending_input_tokens: 0,
            pending_output_tokens: 0,
            pending_cache_read_tokens: 0,
            local_tool_calls: 0,
            server_tool_calls: 0,
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// 吸收一个 AgentEvent，返回需要持久化的 TurnRecord（当 turn 收尾时）。
    pub fn consume(&mut self, ev: &AgentEvent) -> Option<TurnRecord> {
        match ev {
            AgentEvent::TextDelta { .. } | AgentEvent::Thinking { .. } => {
                // 文本流不触发 flush，也不影响 turn 边界——pending 与否独立于此
                None
            }
            AgentEvent::ToolStart { server_side, .. } => {
                // 工具发生在 pending turn 之后（Usage 已到达）。计数挂到 pending turn 上。
                if *server_side {
                    self.server_tool_calls += 1;
                } else {
                    self.local_tool_calls += 1;
                }
                None
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                ..
            } => {
                // 先看是否要 flush 已存在的 pending turn——它的 Usage + 之后到达的
                // ToolStart/End 已经全部累积，可以收尾。
                let to_flush = if self.has_pending {
                    Some(self.flush_pending(None))
                } else {
                    None
                };
                // 把这条 Usage 设成新的 pending turn
                self.pending_input_tokens = *input_tokens;
                self.pending_output_tokens = *output_tokens;
                self.pending_cache_read_tokens = *cache_read_tokens;
                self.has_pending = true;
                to_flush
            }
            AgentEvent::Done { stop_reason, .. } => {
                if self.has_pending {
                    Some(self.flush_pending(Some(*stop_reason)))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn flush_pending(&mut self, stop_reason: Option<StopReason>) -> TurnRecord {
        let now = Utc::now().to_rfc3339();
        let rec = TurnRecord {
            turn: self.current_turn,
            started_at: std::mem::replace(&mut self.started_at, now.clone()),
            ended_at: now,
            input_tokens: self.pending_input_tokens,
            output_tokens: self.pending_output_tokens,
            cache_read_tokens: self.pending_cache_read_tokens,
            local_tool_calls: std::mem::take(&mut self.local_tool_calls),
            server_tool_calls: std::mem::take(&mut self.server_tool_calls),
            stop_reason,
        };
        self.current_turn += 1;
        self.has_pending = false;
        self.pending_input_tokens = 0;
        self.pending_output_tokens = 0;
        self.pending_cache_read_tokens = 0;
        rec
    }
}

// 防 unused warning——后续 pipeline 可能会用 HashMap 聚合多 run
#[allow(dead_code)]
fn _silence_hashmap_warning() -> HashMap<String, u32> {
    HashMap::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::types::{AgentEvent, PipelineKind};
    use serde_json::json;

    fn usage(input: u32, output: u32, cache_read: u32) -> AgentEvent {
        AgentEvent::Usage {
            run_id: "r".into(),
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_write_tokens: 0,
        }
    }
    fn tool_start(server_side: bool) -> AgentEvent {
        AgentEvent::ToolStart {
            run_id: "r".into(),
            tool_use_id: "t".into(),
            name: "x".into(),
            input: json!({}),
            server_side,
        }
    }
    fn done(stop: StopReason) -> AgentEvent {
        AgentEvent::Done {
            run_id: "r".into(),
            stop_reason: stop,
            turns: 1,
        }
    }

    #[test]
    fn single_text_only_turn_records_at_done() {
        // 流：TextDelta → Usage → Done
        let mut acc = TurnAccumulator::new("r");
        let _ = acc.consume(&AgentEvent::TextDelta {
            run_id: "r".into(),
            delta: "hi".into(),
        });
        // Usage 暂存，不 emit
        assert!(acc.consume(&usage(10, 5, 0)).is_none());
        // Done 触发 flush
        let rec = acc.consume(&done(StopReason::EndTurn)).unwrap();
        assert_eq!(rec.turn, 1);
        assert_eq!(rec.input_tokens, 10);
        assert_eq!(rec.output_tokens, 5);
        assert_eq!(rec.local_tool_calls, 0);
        assert_eq!(rec.stop_reason, Some(StopReason::EndTurn));
    }

    #[test]
    fn tool_only_turn_no_text_still_records_with_correct_counts() {
        // 关键 bug 修复点：tool-only turn 流是
        //   Usage(turn 1)        ← Usage 没有先 TextDelta
        //   ToolStart(t1)        ← 工具计数应当属于 turn 1
        //   ToolEnd(t1)
        //   TextDelta(turn 2)    ← turn 2 的回答
        //   Usage(turn 2)        ← 触发 flush turn 1
        //   Done                 ← flush turn 2
        let mut acc = TurnAccumulator::new("r");
        // turn 1: 纯 tool_use, 无 text
        assert!(acc.consume(&usage(20, 3, 0)).is_none());
        assert!(acc.consume(&tool_start(false)).is_none());
        // turn 2: text 回答
        let _ = acc.consume(&AgentEvent::TextDelta {
            run_id: "r".into(),
            delta: "answer".into(),
        });
        // 第二次 Usage：flush turn 1
        let rec1 = acc.consume(&usage(30, 8, 5)).unwrap();
        assert_eq!(rec1.turn, 1);
        assert_eq!(rec1.input_tokens, 20);
        assert_eq!(rec1.output_tokens, 3);
        // **核心断言**：tool 计数留在 turn 1，不是 turn 2
        assert_eq!(rec1.local_tool_calls, 1);
        assert_eq!(rec1.stop_reason, None); // 中间 turn 没有 stop_reason

        let rec2 = acc.consume(&done(StopReason::EndTurn)).unwrap();
        assert_eq!(rec2.turn, 2);
        assert_eq!(rec2.input_tokens, 30);
        assert_eq!(rec2.cache_read_tokens, 5);
        assert_eq!(rec2.local_tool_calls, 0);
        assert_eq!(rec2.stop_reason, Some(StopReason::EndTurn));
    }

    #[test]
    fn server_side_tool_counted_separately() {
        let mut acc = TurnAccumulator::new("r");
        let _ = acc.consume(&usage(10, 5, 0));
        let _ = acc.consume(&tool_start(true)); // server-side
        let _ = acc.consume(&tool_start(false)); // local
        let rec = acc.consume(&done(StopReason::EndTurn)).unwrap();
        assert_eq!(rec.local_tool_calls, 1);
        assert_eq!(rec.server_tool_calls, 1);
    }

    #[test]
    fn done_without_pending_usage_emits_nothing() {
        // 如果 run 失败前没收到任何 Usage，Done 不该 emit 空记录
        let mut acc = TurnAccumulator::new("r");
        assert!(acc.consume(&done(StopReason::EndTurn)).is_none());
    }

    #[test]
    fn turn_increment_continues_correctly() {
        let mut acc = TurnAccumulator::new("r");
        // turn 1
        let _ = acc.consume(&usage(1, 1, 0));
        let _ = acc.consume(&tool_start(false));
        // turn 2 starts → flush turn 1
        let r1 = acc.consume(&usage(2, 2, 0)).unwrap();
        assert_eq!(r1.turn, 1);
        let _ = acc.consume(&tool_start(false));
        // turn 3 starts → flush turn 2
        let r2 = acc.consume(&usage(3, 3, 0)).unwrap();
        assert_eq!(r2.turn, 2);
        let r3 = acc.consume(&done(StopReason::EndTurn)).unwrap();
        assert_eq!(r3.turn, 3);
        assert_eq!(r3.local_tool_calls, 0);
    }

    #[test]
    fn pipeline_kind_unused_import_silenced() {
        // 占位：保留 PipelineKind 引用避免 unused warning
        let _ = PipelineKind::Chat;
    }
}

pub fn stop_reason_str(sr: StopReason) -> &'static str {
    match sr {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::StopSequence => "stop_sequence",
        StopReason::MaxTurns => "max_turns",
        StopReason::SearchBudgetExhausted => "search_budget_exhausted",
        StopReason::Refusal => "refusal",
        StopReason::PauseTurn => "pause_turn",
    }
}
