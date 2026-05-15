//! Review 流水线——审判一条具体的 analysis_record。
//!
//! 流程：
//! 1. 从 SQLite 拉 record + 上下文（市场/行情/持仓/事件链/记忆/学习/近期记录）
//! 2. build_review_prompt → agent loop（外部审稿人视角）→ parse_review
//! 3. 把 review 结果写回对应 record（status/nextReviewAt/review 字段）
//! 4. append "review" 类型的 chat_message
//! 5. 如果 record 关联了仍 open 的仓位 → 追加 "reviewed"（始终）+ "invalidated"（thesisStatus 命中）事件
//! 6. emit review-published

use crate::agent::types::PipelineKind;
use crate::db;
use crate::infrastructure::account::watchlist;
use crate::learning::build_learning_profile;
use crate::pipeline::runner::run_agent_text;
use crate::pipeline::{
    apply_closes_in_memory, collect_relevant_codes, emit_status, fetch_market_overview,
    fetch_quotes_with_visibility, new_id, now_iso, read_investor_memory,
    read_position_events_for_open, read_positions, read_recent_records, PositionClose,
    EVENT_POSITIONS_CHANGED, EVENT_REVIEW_PUBLISHED, SIMULATION_INITIAL_CASH,
};
use crate::prompt::{build_review_prompt, parse_review, ReviewPromptInput};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter};

/// 同时只允许一条 review 在跑（briefing 也用类似锁，但和 review 独立——可以并发）
static REVIEW_RUNNING: AtomicBool = AtomicBool::new(false);
struct ReviewGuard;
impl Drop for ReviewGuard {
    fn drop(&mut self) {
        REVIEW_RUNNING.store(false, Ordering::SeqCst);
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewPipelineResult {
    pub message_id: String,
    pub record_id: String,
}

#[tauri::command]
pub async fn run_review_now(
    app: AppHandle,
    record_id: String,
) -> Result<ReviewPipelineResult, String> {
    if REVIEW_RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("已有 review 在运行".into());
    }
    let _guard = ReviewGuard;
    run_review_inner(&app, record_id).await
}

async fn run_review_inner(
    app: &AppHandle,
    record_id: String,
) -> Result<ReviewPipelineResult, String> {
    // 1. 找到 record
    emit_status(app, "loading", "正在准备复盘上下文");
    let mut all_records = read_recent_records(app, 300)?;
    let record = all_records
        .iter()
        .find(|r| r.id == record_id)
        .cloned()
        .ok_or_else(|| format!("未找到 analysis_record {}", record_id))?;

    // 2. 上下文
    let positions = read_positions(app).unwrap_or_default();
    let position_events = read_position_events_for_open(app, &positions);
    let memory = read_investor_memory(app);
    let watchlist = watchlist::list_strings();

    // 复盘需要原 record 里 targetStocks 的行情；与自选股 + 持仓 code 合并
    let target_codes: Vec<String> = record
        .result
        .trade_plan
        .target_stocks
        .iter()
        .filter_map(|s| s.code.clone())
        .filter(|c| is_a_share_code(c))
        .collect();
    let mut all_codes: std::collections::HashSet<String> =
        collect_relevant_codes(&watchlist, &positions)
            .into_iter()
            .collect();
    for c in target_codes {
        all_codes.insert(c);
    }
    let quotes_status =
        fetch_quotes_with_visibility(app, "review", all_codes.into_iter().collect()).await;
    let quotes_availability = quotes_status.to_prompt_section();
    let quotes = &quotes_status.quotes;
    let market = fetch_market_overview(app).await;

    // 学习画像 + 近期记录（去掉自己）
    let learning =
        build_learning_profile(&all_records, &positions, quotes, SIMULATION_INITIAL_CASH);
    let recent_records: Vec<_> = all_records
        .iter()
        .filter(|r| r.id != record_id)
        .take(12)
        .cloned()
        .collect();

    let prompt_input = ReviewPromptInput {
        record: &record,
        market_overview: market.as_ref(),
        watchlist_quotes: quotes,
        simulated_positions: &positions,
        position_events: &position_events,
        investor_memory: Some(&memory),
        learning_profile: Some(&learning),
        recent_records: &recent_records,
        allow_external_research: true,
        quotes_availability: quotes_availability.as_deref(),
    };
    let prompt = build_review_prompt(&prompt_input);

    // 3. 调 agent loop——review 是审稿任务，强制 adaptive thinking（summarized 让 UI 看到
    //    思考流）+ xhigh effort（Anthropic 文档对 agentic 推理任务的推荐起点）。这两个
    //    override 比 channel 默认更严格，保证 review 始终走深度推理路径。
    emit_status(
        app,
        "running",
        &format!("Agent 正在复盘：{}", record.item.title),
    );
    let review_thinking = Some(crate::agent::types::ThinkingConfig::Adaptive {
        display: Some(crate::agent::types::ThinkingDisplay::Summarized),
    });
    let final_text = run_agent_text(
        app,
        PipelineKind::Review,
        prompt,
        Some(review_thinking),
        Some(crate::agent::types::EffortLevel::XHigh),
        Some(record_id.clone()),
    )
    .await?;
    let review = parse_review(&final_text).map_err(|e| format!("review 解析失败：{}", e))?;

    // 4-6. 单事务组装：records + chat_message + position_events + (optional) positions
    //
    // 之前是 4 个独立事务串行（replace_records / append_chat / append_event / positions 覆盖）——
    // 中途失败留 torn-state（"已标 reviewed 但用户看不到消息"或"标 invalidated 但仓位还 open"）。
    // 现在用 db::commit_review 一把事务搞完，要么全成要么全无。
    emit_status(app, "writing", "正在落盘复盘结果");

    // 4. 更新 records（in-memory）
    if let Some(target) = all_records.iter_mut().find(|r| r.id == record_id) {
        target.next_review_at = review.next_review_at.clone();
        target.review = Some(review.clone());
    }
    let records_payload: Vec<Value> = all_records
        .iter()
        .filter_map(|r| serde_json::to_value(r).ok())
        .collect();

    // 5. review chat_message
    let message_id = new_id();
    let header = format!(
        "**复盘｜{}**\n\n状态：{}\n\n{}",
        record.item.title, review.thesis_status, review.summary
    );
    let chat_msg = json!({
        "id": message_id,
        "createdAt": now_iso(),
        "role": "assistant",
        "kind": "review",
        "contentMd": header,
        "contentJson": { "review": review },
        "sourceTaskId": null,
        "sourceNewsIds": null,
        "sourceRecordId": record_id,
    });

    // 6. 关联 open 仓位 → reviewed（始终）+ invalidated（thesisStatus 命中）+ closed 事件，
    //    invalidated 时还要把仓位翻 closed 落盘。所有事件同 occurred_at 起点 + 1ms 递增
    //    保证回放顺序稳定。
    let mut position_events: Vec<Value> = Vec::new();
    let mut positions_after: Option<Vec<Value>> = None;
    let mut invalidated_with_position = false;
    if let Some(linked) = positions
        .iter()
        .find(|p| p.source_analysis_id == record_id && p.status == "open")
        .cloned()
    {
        let t0 = now_iso();
        let mut t = t0.clone();
        let next_ms = |current: &str, delta_ms: i64| -> String {
            chrono::DateTime::parse_from_rfc3339(current)
                .ok()
                .and_then(|d| d.checked_add_signed(chrono::Duration::milliseconds(delta_ms)))
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| current.to_string())
        };

        // reviewed 事件（始终）
        position_events.push(json!({
            "id": new_id(),
            "positionId": linked.id,
            "eventKind": "reviewed",
            "occurredAt": t,
            "sourceKind": "review",
            "sourceRef": record_id,
            "payload": {
                "thesisStatus": review.thesis_status,
                "confidence": review.confidence,
            },
            "agentNoteMd": review.summary,
        }));

        if review.thesis_status == "invalidated" {
            invalidated_with_position = true;
            // invalidated 事件（reviewed 之后 1ms）
            t = next_ms(&t, 1);
            position_events.push(json!({
                "id": new_id(),
                "positionId": linked.id,
                "eventKind": "invalidated",
                "occurredAt": t,
                "sourceKind": "review",
                "sourceRef": record_id,
                "payload": { "thesisStatus": "invalidated" },
                "agentNoteMd": review.summary,
            }));

            // closed 事件（再 +1ms）+ 翻仓位状态
            let exit_price = quotes
                .iter()
                .find(|q| q.code.as_str() == linked.code)
                .and_then(|q| q.price)
                .map(|y| y.value())
                .unwrap_or(linked.entry_price);
            let close = PositionClose {
                position_id: linked.id.clone(),
                reason: "invalidated".into(),
                exit_price,
                source_kind: "review".into(),
                source_ref: Some(record_id.clone()),
                agent_note_md: format!("复盘判定假设证伪，自动平仓退场：{}", review.summary),
            };
            t = next_ms(&t, 1);
            position_events.push(json!({
                "id": new_id(),
                "positionId": close.position_id,
                "eventKind": "closed",
                "occurredAt": t,
                "sourceKind": close.source_kind,
                "sourceRef": close.source_ref,
                "payload": {
                    "reason": close.reason,
                    "exitPrice": close.exit_price,
                },
                "agentNoteMd": close.agent_note_md,
            }));
            // 仓位状态翻 closed（in-memory）→ 整列序列化进 commit
            let updated = apply_closes_in_memory(positions.clone(), &[close], &t);
            positions_after = Some(
                updated
                    .iter()
                    .filter_map(|p| serde_json::to_value(p).ok())
                    .collect(),
            );
        }
    }

    let commit = db::ReviewCommit {
        records: records_payload,
        chat_messages: vec![chat_msg],
        position_events,
        positions: positions_after,
    };
    db::commit_review(app.clone(), commit)?;
    tracing::info!(
        record_id = %record_id,
        message_id = %message_id,
        thesis_status = %review.thesis_status,
        invalidated_close = invalidated_with_position,
        "review 单事务提交成功"
    );
    if invalidated_with_position {
        let _ = app.emit(EVENT_POSITIONS_CHANGED, json!({}));
    }

    // 7. emit
    let _ = app.emit(
        EVENT_REVIEW_PUBLISHED,
        json!({
            "messageId": message_id,
            "recordId": record_id,
        }),
    );
    emit_status(app, "done", &format!("复盘完成：{}", review.thesis_status));

    Ok(ReviewPipelineResult {
        message_id,
        record_id,
    })
}

// `write_review_position_events` 之前是 review 单独写事件的 helper——
// 单事务化后所有事件由 commit_review 统一写，helper 不再需要。

fn is_a_share_code(s: &str) -> bool {
    s.len() == 6 && s.chars().all(|c| c.is_ascii_digit())
}
