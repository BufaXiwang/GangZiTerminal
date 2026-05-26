//! trade_intents + agent_order_intent_index 持久化 —— spec `agent-runtime-module.md §2`。
//!
//! - `insert` / `update_status` / `record_order_index`：operate_account 写流程
//! - `find_by_order`：orderId → intent/episode/run 反查（订单终态 review 路径用，
//!   spec §4 mapping_missing 验收点）
//! - `recover_submitted`：启动恢复 status=submitted intent，按 AccountResultRef /
//!   ToolCall.output_payload_json 推进终态

#![allow(dead_code)] // find_by_order / list_submitted 是 spec 暴露的反查 / 诊断接口，调用方按需启用

use crate::infrastructure::db::helpers::now;
use crate::infrastructure::db::{migrate, open_database};
use crate::domain::agent_runtime::decisions::{
    AccountResultRef, TradeIntent, TradeIntentStatus,
};
use rusqlite::{params, Connection};
use tauri::AppHandle;

fn conn(app: &AppHandle) -> Result<Connection, String> {
    let c = open_database(app)?;
    migrate(&c)?;
    Ok(c)
}

fn status_str(s: TradeIntentStatus) -> &'static str {
    match s {
        TradeIntentStatus::Proposed => "proposed",
        TradeIntentStatus::Submitted => "submitted",
        TradeIntentStatus::Accepted => "accepted",
        TradeIntentStatus::Rejected => "rejected",
        TradeIntentStatus::Executed => "executed",
    }
}

pub fn insert(app: &AppHandle, intent: &TradeIntent) -> Result<(), String> {
    let c = conn(app)?;
    let account_input = serde_json::to_string(&intent.account_input)
        .map_err(|e| format!("account_input 序列化失败：{e}"))?;
    let strategy_ids = serde_json::to_string(&intent.strategy_ids)
        .map_err(|e| format!("strategy_ids 序列化失败：{e}"))?;
    let result_ref = intent
        .account_result_ref
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| format!("result_ref 序列化失败：{e}"))?;
    c.execute(
        "insert into trade_intents(
            intent_id, run_id, episode_id, tool_call_id,
            account_input_json, reason, strategy_ids_json, status,
            account_result_ref_json, created_at, updated_at
         ) values (?1,?2,?3,?4, ?5,?6,?7,?8, ?9, ?10, ?10)",
        params![
            intent.intent_id,
            intent.run_id,
            intent.episode_id,
            intent.tool_call_id,
            account_input,
            intent.reason,
            strategy_ids,
            status_str(intent.status),
            result_ref,
            intent.created_at,
        ],
    )
    .map_err(|e| format!("写 trade_intent 失败：{e}"))?;
    Ok(())
}

pub fn update_status(
    app: &AppHandle,
    intent_id: &str,
    status: TradeIntentStatus,
    result: Option<&AccountResultRef>,
) -> Result<(), String> {
    let c = conn(app)?;
    let result_json = result
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| format!("result_ref 序列化失败：{e}"))?;
    c.execute(
        "update trade_intents
         set status = ?2,
             account_result_ref_json = coalesce(?3, account_result_ref_json),
             updated_at = ?4
         where intent_id = ?1",
        params![intent_id, status_str(status), result_json, now()],
    )
    .map_err(|e| format!("update trade_intent 失败：{e}"))?;
    Ok(())
}

pub fn record_order_index(
    app: &AppHandle,
    order_id: &str,
    intent_id: &str,
    episode_id: &str,
    run_id: &str,
    tool_call_id: Option<&str>,
) -> Result<(), String> {
    let c = conn(app)?;
    c.execute(
        "insert or ignore into agent_order_intent_index(
            order_id, intent_id, episode_id, run_id, tool_call_id, created_at
         ) values (?1,?2,?3,?4,?5,?6)",
        params![order_id, intent_id, episode_id, run_id, tool_call_id, now()],
    )
    .map_err(|e| format!("写 order_intent_index 失败：{e}"))?;
    Ok(())
}

pub fn find_by_order(
    app: &AppHandle,
    order_id: &str,
) -> Result<Option<(String, String, String)>, String> {
    let c = conn(app)?;
    let mut stmt = c
        .prepare(
            "select intent_id, episode_id, run_id from agent_order_intent_index
             where order_id = ?1",
        )
        .map_err(|e| format!("prepare 失败：{e}"))?;
    let mut rows = stmt
        .query(params![order_id])
        .map_err(|e| format!("query 失败：{e}"))?;
    if let Some(r) = rows.next().map_err(|e| format!("next 失败：{e}"))? {
        Ok(Some((
            r.get(0).map_err(|e| e.to_string())?,
            r.get(1).map_err(|e| e.to_string())?,
            r.get(2).map_err(|e| e.to_string())?,
        )))
    } else {
        Ok(None)
    }
}

/// 启动恢复：spec §8 要求扫 status=submitted 的 trade intent。
///
/// 第一阶段策略（保守 fail closed）：
/// - 若已有 `account_result_ref_json` —— 表示 Account 已落事实，按 result 推 accepted/executed
/// - 否则标 rejected，message = `submission_unknown_no_account_effect`，
///   不在没有 AccountResultRef 时反向推断 Account 状态
///
/// 返回 `(scanned, recovered, rejected)`。
pub fn list_submitted(app: &AppHandle) -> Result<Vec<String>, String> {
    let c = conn(app)?;
    let mut stmt = c
        .prepare("select intent_id from trade_intents where status = 'submitted'")
        .map_err(|e| format!("prepare 失败：{e}"))?;
    let mut rows = stmt.query([]).map_err(|e| format!("query 失败：{e}"))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(|e| format!("next 失败：{e}"))? {
        out.push(r.get::<_, String>(0).map_err(|e| e.to_string())?);
    }
    Ok(out)
}

pub fn recover_submitted(app: &AppHandle) -> Result<(u64, u64, u64), String> {
    let c = conn(app)?;
    let mut scanned: u64 = 0;
    let mut recovered: u64 = 0;
    let mut rejected: u64 = 0;
    {
        let mut stmt = c
            .prepare(
                "select intent_id, tool_call_id, account_result_ref_json
                 from trade_intents where status = 'submitted'",
            )
            .map_err(|e| format!("prepare 失败：{e}"))?;
        let mut rows = stmt.query([]).map_err(|e| format!("query 失败：{e}"))?;
        while let Some(r) = rows.next().map_err(|e| format!("next 失败：{e}"))? {
            scanned += 1;
            let intent_id: String = r.get(0).map_err(|e| e.to_string())?;
            let tool_call_id: Option<String> = r.get(1).map_err(|e| e.to_string())?;
            let result_ref: Option<String> = r.get(2).map_err(|e| e.to_string())?;
            let ts = now();
            // spec §2 状态机恢复规则：
            // 1. account_result_ref_json → 按 AccountResultRef 形状分类
            // 2. account_result_ref_json 缺失但有 tool_call_id → 查 agent_tool_calls.output_payload_ref
            //    解析 OperateAccount 结构化 result（spec §2「TradeIntent 恢复不得依赖 outputSummary」，
            //    所以读 output_payload_ref；当前 tool_calls_repo 只存 summary，这里 fallback 把
            //    outputSummary JSON 也当 payload 解析——只信结构化 accept/reject 标志，不信任意文本）
            // 3. 都没有 → fail closed rejected
            let mut parsed: Option<serde_json::Value> = result_ref
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .and_then(|s| serde_json::from_str(s).ok());
            if parsed.is_none() {
                if let Some(tcid) = tool_call_id.as_deref() {
                    if let Some(payload) = read_tool_call_result(&c, tcid) {
                        parsed = Some(payload);
                    }
                }
            }
            let new_status = parsed
                .as_ref()
                .map(classify_recovery)
                .unwrap_or(RecoverStatus::Rejected);
            match new_status {
                RecoverStatus::Accepted => {
                    c.execute(
                        "update trade_intents set status='accepted', updated_at=?2
                         where intent_id = ?1",
                        params![intent_id, ts],
                    )
                    .map_err(|e| format!("recover accepted 失败：{e}"))?;
                    recovered += 1;
                }
                RecoverStatus::Executed => {
                    c.execute(
                        "update trade_intents set status='executed', updated_at=?2
                         where intent_id = ?1",
                        params![intent_id, ts],
                    )
                    .map_err(|e| format!("recover executed 失败：{e}"))?;
                    recovered += 1;
                }
                RecoverStatus::Rejected => {
                    let stub = serde_json::json!({
                        "message": "submission_unknown_no_account_effect"
                    });
                    c.execute(
                        "update trade_intents
                         set status = 'rejected',
                             account_result_ref_json = coalesce(account_result_ref_json, ?2),
                             updated_at = ?3
                         where intent_id = ?1",
                        params![intent_id, stub.to_string(), ts],
                    )
                    .map_err(|e| format!("recover rejected 失败：{e}"))?;
                    rejected += 1;
                }
            }
        }
    }
    Ok((scanned, recovered, rejected))
}

enum RecoverStatus {
    Accepted,
    Executed,
    Rejected,
}

fn read_tool_call_result(c: &Connection, tool_call_id: &str) -> Option<serde_json::Value> {
    // spec agent-runtime-module.md §2「TradeIntent 恢复不得依赖 outputSummary 解析」。
    // 优先读结构化 output_payload_json；如果该列为空，再 trace warn 并放弃（不读 summary）。
    let row: Result<Option<String>, _> = c.query_row(
        "select output_payload_json from agent_tool_calls where tool_call_id = ?1",
        params![tool_call_id],
        |r| r.get::<_, Option<String>>(0),
    );
    let payload_str = row.ok().flatten();
    if payload_str.is_none() {
        tracing::warn!(
            target = "trade_intents.recover",
            tool_call_id,
            "无 output_payload_json，按 spec 不再 fallback 解析 output_summary_json"
        );
    }
    payload_str.and_then(|s| serde_json::from_str(&s).ok())
}

fn classify_recovery(result: &serde_json::Value) -> RecoverStatus {
    if result
        .get("rejection_event_id")
        .or_else(|| result.get("rejectionEventId"))
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        return RecoverStatus::Rejected;
    }
    let order_id = result
        .get("order_id")
        .or_else(|| result.get("orderId"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let fill_ids_non_empty = result
        .get("fill_ids")
        .or_else(|| result.get("fillIds"))
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    let has_position_or_event = result
        .get("position_id")
        .or_else(|| result.get("positionId"))
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false)
        || result
            .get("account_event_ids")
            .or_else(|| result.get("accountEventIds"))
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
    if order_id.is_some() && !fill_ids_non_empty {
        // limit pending：order 已被 Account 受理但未成交
        RecoverStatus::Accepted
    } else if fill_ids_non_empty || has_position_or_event {
        RecoverStatus::Executed
    } else {
        // 没有任何 Account 副作用证据 —— fail closed rejected
        RecoverStatus::Rejected
    }
}
