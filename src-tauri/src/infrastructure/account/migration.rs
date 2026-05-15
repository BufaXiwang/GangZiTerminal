//! 一次性 Legacy positions → events 补偿。
//!
//! 旧版本 `simulated_positions` 表里可能有"无 events 的 positions"——
//! 那时 cash 派生走 `cash_delta_from_legacy_record` fallback（直接从 position 字段算）。
//! 新版只信事件链；为保历史数据不丢，启动时扫一遍：
//!
//! 1. 列出所有 positions
//! 2. 列出所有 events，按 position_id 聚合
//! 3. **对无事件的 position**：
//!    - 反向生成 `Opened` event（用 entry_price / original_shares / entry_at / commission(约) 反推）
//!    - 若 status="closed"，再补 `Closed` event（用 exit_price / exit_at / close_reason / commission+tax(约) 反推）
//! 4. append 这些事件到 `position_events`
//!
//! commission / stamp_tax 用规则函数估算（不知道当时实际收的多少，按 0.025%/0.1% 算）。
//! 这是合理近似——legacy 数据本来就没记费用，估算让 cash 派生数字稳定。

use crate::db;
use crate::domain::account::rules::{commission, stamp_tax};
use crate::domain::account::types::{
    CloseReason, EventSource, PositionEvent, PositionEventKind, PositionId,
};
use crate::domain::shared::{Shares, Yuan};
use std::collections::HashSet;
use tauri::AppHandle;

use super::repository::{occurred_at_to_rfc3339, parse_rfc3339};

/// 启动时调一次。返回补偿事件数（用于日志）。
///
/// 失败仅日志告警，不抛出——migration 失败也不应该阻塞启动。
pub fn migrate_legacy_positions(app: &AppHandle) -> Result<usize, String> {
    // 1. 读 positions
    let raw_positions = db::list_simulated_positions(app.clone())?;
    if raw_positions.is_empty() {
        return Ok(0);
    }
    let position_ids: Vec<String> = raw_positions
        .iter()
        .filter_map(|v| v.get("id").and_then(|x| x.as_str()).map(String::from))
        .collect();
    if position_ids.is_empty() {
        return Ok(0);
    }

    // 2. 读 events 并按 position_id 聚合
    let raw_events = db::list_position_events_batch(app.clone(), position_ids.clone())?;
    let mut positions_with_events: HashSet<String> = HashSet::new();
    for e in &raw_events {
        if let Some(pid) = e.get("positionId").and_then(|v| v.as_str()) {
            positions_with_events.insert(pid.to_string());
        }
    }

    // 3. 对无事件的 position 反向生成
    let mut count = 0;
    for raw in raw_positions {
        let Some(id) = raw.get("id").and_then(|v| v.as_str()).map(String::from) else {
            continue;
        };
        if positions_with_events.contains(&id) {
            continue;
        }

        if let Err(e) = backfill_one(app, &id, &raw) {
            tracing::warn!(position_id = %id, error = %e, "legacy migration: 补偿失败，跳过");
            continue;
        }
        count += 1;
    }

    if count > 0 {
        tracing::info!(count, "legacy migration: 已为无事件的 position 补偿 events");
    }
    Ok(count)
}

fn backfill_one(app: &AppHandle, id: &str, raw: &serde_json::Value) -> Result<(), String> {
    let entry_price = raw
        .get("avgEntryPrice")
        .or_else(|| raw.get("entryPrice"))
        .and_then(|v| v.as_f64())
        .ok_or("缺 entryPrice")?;
    let shares_value = raw
        .get("originalShares")
        .or_else(|| raw.get("shares"))
        .and_then(|v| v.as_i64())
        .ok_or("缺 shares")?;
    if shares_value <= 0 || entry_price <= 0.0 {
        return Err("entry_price 或 shares 非法".into());
    }
    let entry_at_str = raw
        .get("entryAt")
        .and_then(|v| v.as_str())
        .ok_or("缺 entryAt")?;
    let source_analysis_id = raw
        .get("sourceAnalysisId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let status = raw.get("status").and_then(|v| v.as_str()).unwrap_or("open");

    let entry_price_y = Yuan::new(entry_price).map_err(|e| e.to_string())?;
    let shares_typed = Shares::from_unchecked(shares_value);
    let entered_at = parse_rfc3339(entry_at_str);
    let commission_amount = commission(entry_price_y, shares_typed);

    // 1) 写 Opened event
    let opened = PositionEvent {
        id: uuid::Uuid::new_v4().to_string(),
        position_id: PositionId::from_string(id.to_string()),
        kind: PositionEventKind::Opened {
            entry_price: entry_price_y,
            shares: shares_typed,
            commission: commission_amount,
        },
        occurred_at: entered_at,
        source: if source_analysis_id.is_empty() {
            EventSource::System
        } else {
            EventSource::Briefing {
                analysis_id: source_analysis_id,
            }
        },
        agent_note_md: "(legacy migration: 反向生成 opened)".into(),
    };
    write_event(app, &opened)?;

    // 2) 如果 closed，写 Closed event
    if status == "closed" {
        let exit_price = raw
            .get("exitPrice")
            .and_then(|v| v.as_f64())
            .unwrap_or(entry_price);
        let exit_at_str = raw
            .get("exitAt")
            .and_then(|v| v.as_str())
            .unwrap_or(entry_at_str);
        let close_reason_str = raw
            .get("closeReason")
            .and_then(|v| v.as_str())
            .unwrap_or("manual");
        let exit_price_y = Yuan::new(exit_price).map_err(|e| e.to_string())?;
        let exit_at = parse_rfc3339(exit_at_str);
        let commission_close = commission(exit_price_y, shares_typed);
        let stamp_tax_close = stamp_tax(exit_price_y, shares_typed);

        let reason = match close_reason_str {
            "stop_loss" => CloseReason::StopLoss,
            "take_profit" => CloseReason::TakeProfit,
            "time_stop" => CloseReason::TimeStop,
            "invalidated" => CloseReason::Invalidated,
            _ => CloseReason::Manual,
        };

        let closed = PositionEvent {
            id: uuid::Uuid::new_v4().to_string(),
            position_id: PositionId::from_string(id.to_string()),
            kind: PositionEventKind::Closed {
                exit_price: exit_price_y,
                shares: shares_typed,
                reason,
                commission: commission_close,
                stamp_tax: stamp_tax_close,
            },
            occurred_at: exit_at,
            source: EventSource::System,
            agent_note_md: "(legacy migration: 反向生成 closed)".into(),
        };
        write_event(app, &closed)?;
    }

    let _ = occurred_at_to_rfc3339; // silence unused warning if formatter not used
    Ok(())
}

fn write_event(app: &AppHandle, event: &PositionEvent) -> Result<(), String> {
    // 借 repository 里的 event_to_db_json，但那是 pub(crate) 没暴露——
    // 这里手动一份小拷贝（仅 migration 用，量小不复杂）
    let (kind_tag, payload) = match &event.kind {
        PositionEventKind::Opened {
            entry_price,
            shares,
            commission,
        } => (
            "opened",
            serde_json::json!({
                "entryPrice": entry_price.value(),
                "shares": shares.value(),
                "commission": commission.value(),
            }),
        ),
        PositionEventKind::Closed {
            exit_price,
            shares,
            reason,
            commission,
            stamp_tax,
        } => (
            "closed",
            serde_json::json!({
                "exitPrice": exit_price.value(),
                "shares": shares.value(),
                "reason": reason.as_str(),
                "commission": commission.value(),
                "stampTax": stamp_tax.value(),
            }),
        ),
        _ => return Err("migration 只生成 opened/closed".into()),
    };
    let (source_kind, source_ref): (&str, Option<String>) = match &event.source {
        EventSource::Briefing { analysis_id } => ("briefing", Some(analysis_id.clone())),
        EventSource::System => ("system", None),
        _ => ("manual", None),
    };
    let v = serde_json::json!({
        "id": event.id,
        "positionId": event.position_id.as_str(),
        "eventKind": kind_tag,
        "occurredAt": event.occurred_at.to_rfc3339(),
        "sourceKind": source_kind,
        "sourceRef": source_ref,
        "payload": payload,
        "agentNoteMd": event.agent_note_md,
    });
    db::append_position_event(app.clone(), v).map(|_| ())
}
