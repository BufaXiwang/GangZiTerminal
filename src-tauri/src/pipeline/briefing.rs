//! Briefing 流水线——从 pending 资讯 → buffer claim → Agent → 落盘 → 模拟开仓 → emit。
//!
//! 全部后端：上下文从 SQLite + 行情接口实时读，prompt 在 Rust 构建，agent loop 调 provider，
//! 解析、记忆合并、记录写入、仓位开仓、事件写入、消息发布、资讯状态更新一气呵成。
//! 完成后 emit `briefing-published`。前端只负责"按按钮"和"听事件 refetch"。

use crate::agent::types::PipelineKind;
use crate::agent_io::{BriefingTradeCall, SimulatedPosition, StoredAnalysisRecord, StoredNewsItem};
use crate::db;
use crate::domain::quotes::StockQuote;
use crate::infrastructure::account::watchlist;
use crate::learning::build_learning_profile;
use crate::memory::merge_investor_memory;
use crate::pipeline::runner::run_agent_text;
use crate::pipeline::{
    collect_relevant_codes, emit_status, fetch_market_overview, fetch_quotes_with_visibility,
    new_id, now_iso, now_millis, read_investor_memory, read_position_events_for_open,
    read_positions, read_recent_briefings, read_recent_records, save_last_briefing_at,
    EVENT_BRIEFING_PUBLISHED, EVENT_POSITIONS_CHANGED, KEY_INVESTOR_MEMORY, KEY_LAST_BRIEFING_AT,
    SIMULATION_INITIAL_CASH,
};
use crate::prompt::{
    build_briefing_prompt, parse_briefing, trade_call_to_analysis_result, BriefingPromptInput,
};
use crate::trade::{derive_stop_loss, derive_take_profit, derive_trade_weight};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter};

/// 保证同时只有一条 briefing 在跑——并发触发的二次调用直接拒绝。
static BRIEFING_RUNNING: AtomicBool = AtomicBool::new(false);

struct BriefingGuard;
impl Drop for BriefingGuard {
    fn drop(&mut self) {
        BRIEFING_RUNNING.store(false, Ordering::SeqCst);
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum BriefingPipelineResult {
    /// buffer 空，没拿到任何 pending 资讯，没生成 briefing
    Empty,
    /// 正常生成
    Done {
        message_id: String,
        trade_call_count: usize,
        covered_count: usize,
    },
}

#[tauri::command]
pub async fn run_briefing_now(app: AppHandle) -> Result<BriefingPipelineResult, String> {
    if BRIEFING_RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("briefing 已在运行".into());
    }
    let _guard = BriefingGuard;
    run_briefing_inner(&app).await
}

async fn run_briefing_inner(app: &AppHandle) -> Result<BriefingPipelineResult, String> {
    // ---------- 1. claim pending news batch ----------
    emit_status(app, "claiming", "正在 claim pending 资讯");
    let claimed_values = db::claim_pending_news_batch(app.clone(), 60)?;
    if claimed_values.is_empty() {
        emit_status(app, "idle", "buffer 为空，没有可消化资讯");
        save_last_briefing_at(app, now_millis())?; // 防止下一次扫描马上又触发
        return Ok(BriefingPipelineResult::Empty);
    }
    let claimed: Vec<StoredNewsItem> = claimed_values
        .iter()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect();
    let claimed_ids: Vec<String> = claimed.iter().map(|n| n.id.clone()).collect();

    // 失败时统一回滚 claim：单事务化以后只剩 Claimed/AgentDone 两态，都该 revert。
    // commit_briefing 是原子的——它失败时 DB 还是 claim 之前的快照（只是 news 还停在
    // processing），revert_news_to_pending 把它们放回 pending 让下一轮重新分析。
    let result = build_and_run(app, &claimed, &claimed_ids).await;
    match result {
        Ok(r) => Ok(r),
        Err((_stage, msg)) => {
            if let Err(revert_err) = db::revert_news_to_pending(app.clone(), claimed_ids.clone()) {
                tracing::warn!(error = %revert_err, "claim 回滚失败——recover_stale_processing_news 启动时兜底");
            }
            emit_status(app, "error", &format!("briefing 失败：{}", msg));
            // 失败也更新 lastBriefingAt 防止失败风暴
            save_last_briefing_at(app, now_millis())?;
            Err(msg)
        }
    }
}

/// 失败阶段——决定 claim 状态怎么收拾。
/// 单事务化以后只保留 Claimed（claim 完到 agent 之间失败）和 AgentDone（agent 完成
/// 但 commit 失败）两态——commit 是原子的，不再有"消息已存但 consume 没成"这种中间状态。
#[derive(Debug, Clone, Copy)]
enum FailStage {
    Claimed,
    AgentDone,
}

/// Torn-state 窗口走查：
///
/// briefing 的 records / positions / chat_message / news 状态已统一走
/// `db::commit_briefing` 单事务提交。失败时按 `FailStage` 决定是否 revert claim。
/// 迁移前的典型 torn-state 窗口如下，保留在这里是为了说明 commit 边界：
///
/// 1. **records 成功 → positions 失败**：
///    records 已写入但仓位没更新；revert 触发回 pending，下一轮重新生成 records →
///    DB 里残留"幽灵 records"（无 chat_message 引用、UUID 不同所以会重复）。
///
/// 2. **positions 成功 → chat_message 失败**：
///    持仓开了但用户看不到 briefing；下一轮 `try_open_position` 的"同 code 已 open"
///    防重逻辑会避免重复开仓，但 records 仍会重复写一份。
///
/// 3. **chat_message 成功 → mark_news_consumed 失败**：
///    news 卡在 "processing" 状态；`recover_stale_processing_news` 在下次启动时
///    放回 pending（已存在）。这条覆盖到了。
async fn build_and_run(
    app: &AppHandle,
    claimed: &[StoredNewsItem],
    claimed_ids: &[String],
) -> Result<BriefingPipelineResult, (FailStage, String)> {
    // ---------- 2. 读上下文 ----------
    emit_status(app, "loading", "正在准备上下文");
    let positions = read_positions(app).map_err(|e| (FailStage::Claimed, e))?;
    let position_events = read_position_events_for_open(app, &positions);
    // 必须读到表里全部 records——下面会把全部 records + 新增的写回去；只读 50 条会
    // 静默销毁第 51-300 条，包括 scheduler 还没扫到的待复盘记录。
    let records = read_recent_records(app, 300).map_err(|e| (FailStage::Claimed, e))?;
    let memory = read_investor_memory(app);
    let recent_briefings = read_recent_briefings(app, 4);
    let watchlist = watchlist::list_strings();
    let codes = collect_relevant_codes(&watchlist, &positions);
    let quotes_status = fetch_quotes_with_visibility(app, "briefing", codes).await;
    let quotes_availability = quotes_status.to_prompt_section();
    let quotes = &quotes_status.quotes;
    let market = fetch_market_overview(app).await;
    let learning = build_learning_profile(&records, &positions, quotes, SIMULATION_INITIAL_CASH);

    // ---------- 3. 构建 prompt ----------
    let prompt_input = BriefingPromptInput {
        pending_news: claimed,
        market_overview: market.as_ref(),
        watchlist_quotes: quotes,
        simulated_positions: &positions,
        position_events: &position_events,
        recent_briefings: &recent_briefings,
        investor_memory: Some(&memory),
        learning_profile: Some(&learning),
        quotes_availability: quotes_availability.as_deref(),
    };
    let prompt = build_briefing_prompt(&prompt_input);

    // ---------- 4. 调 agent loop ----------
    emit_status(app, "running", "主 Agent 正在生成市场简报…");
    // briefing 是 agentic 决策任务——按 Anthropic 推荐 effort=high（也是 channel 默认）
    let final_text = run_agent_text(app, PipelineKind::Briefing, prompt, None, None, None)
        .await
        .map_err(|e| (FailStage::Claimed, e))?;

    let briefing = parse_briefing(&final_text)
        .map_err(|e| (FailStage::AgentDone, format!("briefing 解析失败：{}", e)))?;

    // ---------- 5. 落盘：记忆 + 记录 + 仓位 + 消息 + consume（单事务）----------
    //
    // D1 走查的修复：原本 5a-5e 是 6 个独立事务串行——任何一步失败都会留下
    // torn-state（最常见的是 records 已写但 chat_message 未写，下一轮新生成
    // records UUID 不同，DB 残留"幽灵 records"）。现在全部包进 commit_briefing
    // 单事务，要么全成要么全无。
    emit_status(app, "writing", "正在落盘 briefing 结果");

    // 5a. 记忆合并（in-memory 计算）
    let new_memory = merge_investor_memory(
        &memory,
        &briefing.memory_updates,
        Some(&briefing.memory_removals),
    );

    // 5b. 准备 records + positions + position_events（in-memory）
    let message_id = new_id();
    let mut new_records: Vec<StoredAnalysisRecord> = Vec::new();
    let mut new_positions: Vec<SimulatedPosition> = positions.clone();
    let mut opened_events: Vec<Value> = Vec::new();
    for (idx, call) in briefing.trade_calls.iter().enumerate() {
        let mut result = trade_call_to_analysis_result(call, &briefing.summary_md);
        let buy_rejected = call.action == "buy"
            && (call.trigger_condition.trim().is_empty()
                || call.invalidation_condition.trim().is_empty());
        if buy_rejected {
            result.summary = format!(
                "[未开仓] Agent 给的 buy 缺触发/失效条件 — {}",
                result.summary
            );
            result.decision = "watch".into();
            result.trade_plan.action = "watch".into();
            result.risks.insert(
                0,
                "Agent 在 briefing 给出 buy 但未提供 triggerCondition 或 invalidationCondition，被代码守卫拒绝开仓".into(),
            );
        }
        let record_item = StoredNewsItem {
            id: format!("briefing-{}-{}", message_id, idx),
            title: format!("{} {}", call.name, call.action),
            source: "Briefing".into(),
            published: Some(now_iso()),
            summary: Some(call.thesis.clone()),
            link: None,
        };
        let record = StoredAnalysisRecord {
            id: new_id(),
            item: record_item,
            result,
            created_at: now_iso(),
            next_review_at: Some(initial_review_at_short()),
            review: None,
        };
        new_records.push(record.clone());
        if !buy_rejected {
            if let Some(opened) = try_open_position(call, &record, &new_positions, &quotes) {
                new_positions.insert(0, opened.position.clone());
                opened_events.push(opened.event_payload);
            }
        }
    }

    // 5c. covered/uncovered 划分
    let claimed_id_set: std::collections::HashSet<&String> = claimed_ids.iter().collect();
    let effective_covered_ids: Vec<String> = briefing
        .covered_news_ids
        .iter()
        .filter(|id| claimed_id_set.contains(id))
        .cloned()
        .collect();
    let covered_set: std::collections::HashSet<&String> = effective_covered_ids.iter().collect();
    let uncovered_ids: Vec<String> = claimed_ids
        .iter()
        .filter(|id| !covered_set.contains(id))
        .cloned()
        .collect();

    // 5d. 准备 chat_messages（briefing 主消息 + 可选 highlight）
    let briefing_msg = json!({
        "id": message_id,
        "createdAt": now_iso(),
        "role": "assistant",
        "kind": "briefing",
        "contentMd": briefing.summary_md,
        "contentJson": {
            "briefing": briefing,
            "memoryUpdates": briefing.memory_updates,
            "memoryRemovals": briefing.memory_removals,
        },
        "sourceTaskId": null,
        "sourceNewsIds": effective_covered_ids,
        "sourceRecordId": null,
    });
    let mut chat_messages: Vec<Value> = vec![briefing_msg];
    if let Some(h) = &briefing.highlight {
        if h.importance == "high" && !h.message.is_empty() {
            let highlight_msg = json!({
                "id": new_id(),
                "createdAt": chrono::Utc::now().checked_add_signed(chrono::Duration::milliseconds(1))
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_else(now_iso),
                "role": "assistant",
                "kind": "highlight",
                "contentMd": h.message,
                "contentJson": { "sourceBriefingId": message_id },
                "sourceTaskId": null,
                "sourceNewsIds": null,
                "sourceRecordId": null,
            });
            chat_messages.push(highlight_msg);
        }
    }

    // 5e. 组装事务 payload
    let positions_changed = new_positions.len() != positions.len();
    let records_payload: Vec<Value> = if !new_records.is_empty() {
        let mut all_records = records.clone();
        for r in new_records.iter().rev() {
            all_records.insert(0, r.clone());
        }
        all_records.truncate(300);
        all_records
            .iter()
            .filter_map(|r| serde_json::to_value(r).ok())
            .collect()
    } else {
        Vec::new()
    };
    let positions_payload: Option<Vec<Value>> = if positions_changed {
        Some(
            new_positions
                .iter()
                .filter_map(|p| serde_json::to_value(p).ok())
                .collect(),
        )
    } else {
        None
    };
    let memory_value =
        serde_json::to_value(&new_memory).map_err(|e| (FailStage::AgentDone, e.to_string()))?;
    let commit = db::BriefingCommit {
        records: if records_payload.is_empty() {
            None
        } else {
            Some(records_payload)
        },
        positions: positions_payload,
        position_events: opened_events,
        chat_messages,
        news_consumed_ids: effective_covered_ids.clone(),
        news_revert_ids: uncovered_ids,
        app_state_writes: vec![
            (KEY_INVESTOR_MEMORY.to_string(), memory_value),
            (KEY_LAST_BRIEFING_AT.to_string(), json!(now_millis())),
        ],
    };

    db::commit_briefing(app.clone(), commit).map_err(|e| (FailStage::AgentDone, e))?;
    tracing::info!(
        message_id = %message_id,
        records = new_records.len(),
        new_positions = if positions_changed { new_positions.len() - positions.len() } else { 0 },
        covered = effective_covered_ids.len(),
        "briefing 单事务提交成功"
    );
    if positions_changed {
        let _ = app.emit(EVENT_POSITIONS_CHANGED, json!({}));
    }

    // ---------- 6. 收尾 ----------
    let _ = app.emit(
        EVENT_BRIEFING_PUBLISHED,
        json!({
            "messageId": message_id,
            "tradeCallCount": briefing.trade_calls.len(),
            "coveredCount": effective_covered_ids.len(),
        }),
    );
    emit_status(
        app,
        "done",
        &format!(
            "briefing 完成：覆盖 {}/{} 条，{} 个交易假设",
            effective_covered_ids.len(),
            claimed.len(),
            briefing.trade_calls.len()
        ),
    );

    Ok(BriefingPipelineResult::Done {
        message_id,
        trade_call_count: briefing.trade_calls.len(),
        covered_count: effective_covered_ids.len(),
    })
}

// ====== 模拟开仓 ======

struct OpenedTrade {
    position: SimulatedPosition,
    event_payload: Value,
}

fn try_open_position(
    call: &BriefingTradeCall,
    record: &StoredAnalysisRecord,
    positions: &[SimulatedPosition],
    quotes: &[StockQuote],
) -> Option<OpenedTrade> {
    if call.action != "buy" {
        return None;
    }
    // prompt 要求 buy 必须给触发条件 + 失效条件——这里做代码层强制。
    // 之前 trade_call_to_analysis_result 会把空字段 fallback 成 placeholder
    // ("等待信号确认。" / "假设证伪条件未给出。")，结果即便 Agent 违反约束也开仓。
    // analysis_record 仍写入审计链（在调用方写过了），只是不开模拟仓。
    if call.trigger_condition.trim().is_empty() || call.invalidation_condition.trim().is_empty() {
        tracing::warn!(
            name = %call.name,
            code = ?call.code,
            trigger_blank = call.trigger_condition.trim().is_empty(),
            invalidation_blank = call.invalidation_condition.trim().is_empty(),
            "拒绝开仓：buy 缺触发/失效条件——agent 违反 prompt 约束"
        );
        return None;
    }
    let code = call.code.as_deref()?;
    if !is_a_share_code(code) {
        return None;
    }
    // 同 code 已 open → 跳过
    if positions
        .iter()
        .any(|p| p.status == "open" && p.code == code)
    {
        return None;
    }
    // 同 sourceAnalysisId 已存在 → 跳过
    if positions.iter().any(|p| p.source_analysis_id == record.id) {
        return None;
    }
    let quote = quotes.iter().find(|q| q.code.as_str() == code)?;
    let price = quote.price?.value();
    if price <= 0.0 {
        return None;
    }
    let plan = &record.result.trade_plan;
    let weight = derive_trade_weight(plan);
    let budget = SIMULATION_INITIAL_CASH * weight;
    let raw_shares = (budget / price).floor() as i64;
    let shares = ((raw_shares / 100).max(1)) * 100;
    let entry_at = now_iso();
    let time_stop_at = crate::pipeline::derive_time_stop_at(&entry_at);
    let position = SimulatedPosition {
        id: new_id(),
        code: code.to_string(),
        name: call.name.clone(),
        entry_price: price,
        shares,
        entry_at,
        exit_price: None,
        exit_at: None,
        close_reason: None,
        thesis: format!(
            "{}｜触发：{}｜失效：{}",
            call.thesis, call.trigger_condition, call.invalidation_condition
        ),
        stop_loss: Some(derive_stop_loss(price, plan)),
        take_profit: Some(derive_take_profit(price, plan)),
        time_stop_at,
        source_analysis_id: record.id.clone(),
        status: "open".into(),
        original_shares: Some(shares),
        current_shares: Some(shares),
        avg_entry_price: Some(price),
    };
    let event_payload = json!({
        "id": new_id(),
        "positionId": position.id,
        "eventKind": "opened",
        "occurredAt": position.entry_at,
        "sourceKind": "briefing",
        "sourceRef": record.id,
        "payload": {
            "code": position.code,
            "name": position.name,
            "shares": position.shares,
            "entryPrice": position.entry_price,
            "stopLoss": position.stop_loss,
            "takeProfit": position.take_profit,
        },
        "agentNoteMd": call.thesis,
    });
    Some(OpenedTrade {
        position,
        event_payload,
    })
}

fn is_a_share_code(s: &str) -> bool {
    s.len() == 6 && s.chars().all(|c| c.is_ascii_digit())
}

fn initial_review_at_short() -> String {
    // 短期假设：1 天后复盘
    let next = chrono::Utc::now() + chrono::Duration::days(1);
    next.to_rfc3339()
}
