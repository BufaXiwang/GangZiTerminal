//! Reflection pipeline（v3 expectation-driven）。
//!
//! 见 docs/design/agent-v3-expectation-driven.md § 5.4 + § 8。
//!
//! 触发：scheduler 每交易日 15:30 调一次 / Settings 立即触发按钮调一次。
//!
//! 流程：
//! 1. `expectation_review::run` 自动判定所有 pending expectations 的 hit/miss/expired
//!    + 写 lessons（observation 由代码生成，takeaway 留空）+ heuristics application_count 累加
//!    + missed → 自动平仓关联 position
//! 2. `heuristic_emerge::run` 尝试从最近 lessons emerge 新 heuristics
//! 3. **Phase 2 LLM takeaway 填充**——若 provider 可用，对当天新生成且 takeaway 为空
//!    的 lessons 调一轮 LLM 批量给出 takeaway（一句话，可反复应用的判断）
//! 4. 落一条 reflection episode 到 agent_episodes 表（trigger_kind="reflection"）

use crate::domain::agent::lesson::Lesson;
use crate::domain::agent::types::{
    AgentEvent, AgentOptions, AgentRequest, Block, ContextBudget, Message, PipelineKind, Role,
    StopReason, SystemBlock,
};
use crate::infrastructure::agent::lesson_repo;
use crate::pipeline::agent::config::{build_provider_for_channel, read_agent_config};
use crate::pipeline::agent::loop_::run_agent;
use crate::pipeline::agent::tools::{ToolContext, ToolRegistry};
use crate::pipeline::agent::{expectation_review, heuristic_emerge, observer};
use serde_json::Value;
use std::sync::Arc;
use tauri::AppHandle;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct ReflectionResult {
    pub run_id: String,
    pub outcome_summary: String,
    pub thesis_count: usize, // 保留字段名兼容旧前端；语义改为 expectations_reviewed
}

/// 触发一次收盘复盘——可由 scheduler 15:30 tick / Settings 立即按钮调。
pub async fn run_close_reflection(
    app: AppHandle,
    _registry: Arc<ToolRegistry>,
) -> Result<ReflectionResult, String> {
    let run_id = uuid::Uuid::new_v4().to_string();
    let started_at = chrono::Utc::now().to_rfc3339();
    let _ = crate::infrastructure::agent::repository::insert_agent_episode_start(
        &app,
        &run_id,
        "reflection",
        Some("close-15:30"),
        "none",
        "none",
        &started_at,
        None,
        None,
    );

    // 1. 自动 review pending expectations（missed 自动平仓事件源带本次 episode_id）
    let review = expectation_review::run(&app, Some(run_id.clone())).await?;

    // 2. 尝试 emerge 新 heuristic
    let emerge = heuristic_emerge::run(&app)?;

    // 3. Phase 2：LLM 给最近 takeaway 空的 lessons 补一句话总结（失败不阻断）
    let takeaways_filled = match fill_lesson_takeaways(&app, &run_id).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "fill_lesson_takeaways 失败，跳过");
            0
        }
    };

    let outcome = format!(
        "Review: examined={}, hit={}, missed={}, expired={}, lessons_written={}, \
         heuristic_applications={}, positions_auto_closed={}. \
         Emerge: clusters={}, new_heuristics={}, duplicates_skipped={}. \
         Takeaways: filled={}.",
        review.examined,
        review.hit,
        review.missed,
        review.expired,
        review.lessons_written,
        review.heuristic_applications_recorded,
        review.positions_auto_closed,
        emerge.clusters_found,
        emerge.heuristics_created,
        emerge.skipped_duplicates,
        takeaways_filled,
    );

    let ended_at = chrono::Utc::now().to_rfc3339();
    let _ = crate::infrastructure::agent::repository::finalize_agent_episode(
        &app,
        &run_id,
        &ended_at,
        0,
        0, 0, 0, 0, 0, 0,
        Some("auto_review_complete"),
        None,
        None,
        Some(&outcome),
    );

    tracing::info!(
        run_id = %run_id,
        outcome = %outcome,
        "Close reflection 完成"
    );

    Ok(ReflectionResult {
        run_id,
        outcome_summary: outcome,
        thesis_count: review.examined,
    })
}

/// Phase 2: 批量调 LLM 给最近 takeaway 为空的 lessons 填一句话总结。
///
/// 用一次 LLM 调用处理一批（最多 20 条）—— provider 拒绝 JSON / 字段不全 → 跳过本条不阻断。
/// 失败（provider 不可用、网络挂、解析挂）→ 上抛错由调用方决定是否阻断；这里被 try 包住。
async fn fill_lesson_takeaways(app: &AppHandle, run_id: &str) -> Result<usize, String> {
    let pending = lesson_repo::list_recent_with_empty_takeaway(app, 20)?;
    if pending.is_empty() {
        return Ok(0);
    }

    let cfg = read_agent_config(app);
    let Ok((channel_ref, model_ref)) = cfg.resolve_pipeline(PipelineKind::Chat) else {
        tracing::info!("无 chat channel 配置，跳过 takeaway 填充");
        return Ok(0);
    };
    let channel = channel_ref.clone();
    let model = model_ref.to_string();

    let context_text = build_takeaway_prompt(&pending);
    let req = AgentRequest {
        system: vec![SystemBlock {
            text: TAKEAWAY_SYSTEM.to_string(),
            cache_control: false,
        }],
        tools: vec![],
        messages: vec![Message {
            role: Role::User,
            content: vec![Block::Text {
                text: context_text,
                cache_control: false,
            }],
        }],
        options: AgentOptions {
            model: model.clone(),
            max_tokens: 2048,
            temperature: Some(0.3),
            top_p: None,
            thinking: channel.thinking_config(),
            effort: channel.default_effort,
            max_turns: 1,
            stop_sequences: vec![],
            tool_timeout_secs: Some(cfg.agent.tool_timeout_secs),
        },
        budget: ContextBudget {
            soft_limit_tokens: cfg.agent.context_soft_limit_tokens,
            hard_limit_tokens: cfg.agent.context_hard_limit_tokens,
            compact_keep_last_n: cfg.agent.compact_keep_last_n_turns,
            max_search_calls: 0,
        },
        trigger_message_id: None,
        pipeline: PipelineKind::Chat,
    };

    let provider = build_provider_for_channel(&channel)
        .map_err(|e| format!("构建 provider 失败：{e}"))?;
    let registry = Arc::new(ToolRegistry::new());

    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let collector = tokio::spawn(async move {
        let mut answer = String::new();
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::TextDelta { delta, .. } = &ev {
                answer.push_str(delta);
            }
        }
        answer
    });
    let ctx = ToolContext {
        run_id: format!("{run_id}::takeaways"),
    };
    let summary = run_agent(provider, None, registry, req, ctx, tx)
        .await
        .map_err(|e| format!("takeaway run_agent 失败：{e}"))?;
    if !matches!(summary.stop_reason, StopReason::EndTurn | StopReason::StopSequence) {
        // 提前停了——能拿到什么算什么
    }
    let answer = collector.await.map_err(|e| format!("collector join 失败：{e}"))?;

    parse_and_apply_takeaways(app, &pending, &answer)
}

const TAKEAWAY_SYSTEM: &str = r#"你是 GangZiTerminal 的复盘助手。
用户给你一批刚刚完成的 lesson（每个有 id / outcome / observation），
要你为每条产出一句简短的 takeaway——「可反复应用于未来场景的判断」。

输出严格 JSON，不要前缀后缀文字：

{"takeaways": [
  {"id": "<lesson_id>", "takeaway": "<≤80 字简短判断>"},
  ...
]}

规则：
- 一句话，≤80 字，普通中文
- 写「下次遇到 X 时该 / 不该 Y」式的可操作判断，不要写「市场情绪」「需要观察」这类空话
- 命中（hit）的 lesson：提炼为什么这个判断奏效
- 未中（miss/expired）的 lesson：提炼什么信号被高估 / 漏看了
- 不需要的 lesson 可省略（不返回 entry 即可，不要 takeaway 字段写空字符串）
"#;

fn build_takeaway_prompt(lessons: &[Lesson]) -> String {
    let mut s = String::with_capacity(1024);
    s.push_str("以下是今天刚生成的 lessons，请按上面 JSON 格式给出 takeaways：\n\n");
    for l in lessons {
        s.push_str(&format!(
            "- id: {}\n  outcome: {}\n  code: {}\n  observation: {}\n\n",
            l.id.as_str(),
            l.outcome.as_str(),
            l.code.as_str(),
            l.observation,
        ));
    }
    s
}

fn parse_and_apply_takeaways(
    app: &AppHandle,
    pending: &[Lesson],
    raw: &str,
) -> Result<usize, String> {
    // 容错：LLM 可能在 JSON 外裹 ```json ... ``` 或前缀文字
    let trimmed = raw.trim();
    let json_str = extract_json_block(trimmed).unwrap_or(trimmed);
    let parsed: Value = serde_json::from_str(json_str)
        .map_err(|e| format!("解析 LLM takeaway JSON 失败：{e}; raw={}", &raw[..raw.len().min(200)]))?;
    let arr = parsed
        .get("takeaways")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "缺少 takeaways 数组".to_string())?;

    let pending_ids: std::collections::HashSet<&str> =
        pending.iter().map(|l| l.id.as_str()).collect();
    let mut filled = 0;
    for entry in arr {
        let Some(id) = entry.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(takeaway) = entry.get("takeaway").and_then(|v| v.as_str()) else {
            continue;
        };
        // 防御：LLM 可能虚构 id，丢弃不在 pending 集合的
        if !pending_ids.contains(id) {
            continue;
        }
        let lesson_id = crate::domain::agent::lesson::LessonId::from_string(id.to_string());
        match lesson_repo::fill_takeaway_if_empty(app, &lesson_id, takeaway.trim()) {
            Ok(true) => filled += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(lesson = %id, error = %e, "fill_takeaway 失败"),
        }
    }
    Ok(filled)
}

fn extract_json_block(s: &str) -> Option<&str> {
    if let Some(rest) = s.strip_prefix("```json") {
        rest.trim_start_matches('\n').strip_suffix("```").map(|x| x.trim())
    } else if let Some(rest) = s.strip_prefix("```") {
        rest.trim_start_matches('\n').strip_suffix("```").map(|x| x.trim())
    } else {
        None
    }
}

// 标记符 observer 模块依赖移除——不再用 observer::start_episode（v2 LLM-driven 路径）
fn _observer_marker() {
    let _ = observer::AGENT_EVENT;
}
