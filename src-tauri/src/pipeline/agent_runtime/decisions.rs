//! TradeIntent / DecisionEpisode pipeline helpers —— spec `agent-runtime-module.md §2`。
//!
//! 把 operate_account dispatch 包裹一层：在调用 Account 写 API 前后维护
//! `TradeIntent` 状态机（proposed → submitted → accepted/executed/rejected），
//! 命中 orderId 时写 `AgentOrderIntentIndex` 反查表。

use crate::domain::agent_runtime::decisions::{
    AccountResultRef, TradeIntent, TradeIntentStatus,
};
use crate::infrastructure::agent_runtime::trade_intents_repo;
use crate::infrastructure::db::helpers::now;
use serde_json::Value;
use tauri::AppHandle;

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 在 operate_account dispatch 前调用：写 `proposed` 状态的 TradeIntent，
/// 同时按 spec §4「operate_account 创建 TradeIntent(proposed) 后，Runtime
/// 必须把对应 episode 推进到 actionStatus = submitted」立即推进 episode。
///
/// 返回 `intent_id`，dispatch 完成后用 `mark_dispatch_outcome` 推进 TradeIntent。
pub fn open_intent(
    app: &AppHandle,
    run_id: &str,
    episode_id: &str,
    tool_call_id: Option<&str>,
    account_input: Value,
    reason: String,
) -> Result<String, String> {
    use crate::domain::agent_runtime::decisions::EpisodeActionStatus;
    use crate::infrastructure::agent_runtime::episodes_repo;
    let intent_id = new_id();
    let intent = TradeIntent {
        intent_id: intent_id.clone(),
        run_id: run_id.to_string(),
        episode_id: episode_id.to_string(),
        tool_call_id: tool_call_id.map(String::from),
        account_input,
        reason,
        strategy_ids: Vec::new(),
        status: TradeIntentStatus::Proposed,
        account_result_ref: None,
        created_at: now(),
        updated_at: now(),
    };
    trade_intents_repo::insert(app, &intent)?;
    // spec §4：Runtime 接受 operate_account 后立即推进 episode 到 submitted
    let _ = episodes_repo::update_action_status(
        app,
        episode_id,
        EpisodeActionStatus::Submitted,
        None,
    );
    // 进入 submitted —— Account 调用即将开始
    trade_intents_repo::update_status(app, &intent_id, TradeIntentStatus::Submitted, None)?;
    Ok(intent_id)
}

/// dispatch 完成后调用：根据 Account 结果推进状态机 + 反查索引。
///
/// `outcome` 语义：
/// - `Ok(result)` 表示 Account 已接受；按是否有未决订单决定 `accepted` 还是 `executed`
///   （第一阶段：所有非 limit pending 路径视为 `executed`）
/// - `Err(_)` 表示 Account 拒绝或异常 —— 标 `rejected`
pub fn mark_dispatch_outcome(
    app: &AppHandle,
    intent_id: &str,
    episode_id: &str,
    run_id: &str,
    tool_call_id: Option<&str>,
    outcome: DispatchOutcome,
) -> Result<TradeIntentStatus, String> {
    use crate::domain::agent_runtime::decisions::EpisodeActionStatus;
    use crate::infrastructure::agent_runtime::episodes_repo;
    match outcome {
        DispatchOutcome::Executed {
            order_id,
            fill_ids,
            position_id,
            account_event_ids,
            message,
        } => {
            let result = AccountResultRef {
                order_id: order_id.clone(),
                fill_ids: Some(fill_ids),
                position_id: position_id.clone(),
                account_event_ids: Some(account_event_ids),
                trigger_id: None,
                rejection_event_id: None,
                message,
            };
            trade_intents_repo::update_status(
                app,
                intent_id,
                TradeIntentStatus::Executed,
                Some(&result),
            )?;
            // spec §2：「市价即时成交、撤单、保护条件调整等没有后续 orderId 终态
            // 的动作，不写 AgentOrderIntentIndex」。所以 Executed 路径不再写反查
            // 索引；只有 AcceptedPending（limit pending，期待后续 orderId 终态）
            // 才写反查。
            let _ = order_id;
            // Episode 已在 open_intent 阶段推进到 submitted，这里不重复写。
            Ok(TradeIntentStatus::Executed)
        }
        DispatchOutcome::AcceptedPending {
            order_id,
            account_event_ids,
        } => {
            let result = AccountResultRef {
                order_id: Some(order_id.clone()),
                fill_ids: None,
                position_id: None,
                account_event_ids: Some(account_event_ids),
                trigger_id: None,
                rejection_event_id: None,
                message: None,
            };
            trade_intents_repo::update_status(
                app,
                intent_id,
                TradeIntentStatus::Accepted,
                Some(&result),
            )?;
            // spec §2：「反查索引只能在 operate_account 返回 accepted = true 且包含
            // orderId 后写入」。limit pending 才会有后续 orderFilled 终态，要走 review。
            trade_intents_repo::record_order_index(
                app,
                &order_id,
                intent_id,
                episode_id,
                run_id,
                tool_call_id,
            )?;
            Ok(TradeIntentStatus::Accepted)
        }
        DispatchOutcome::Rejected { reason, message } => {
            let result = AccountResultRef {
                order_id: None,
                fill_ids: None,
                position_id: None,
                account_event_ids: None,
                trigger_id: None,
                rejection_event_id: None,
                message: Some(format!("{reason}: {}", message.unwrap_or_default())),
            };
            trade_intents_repo::update_status(
                app,
                intent_id,
                TradeIntentStatus::Rejected,
                Some(&result),
            )?;
            // Spec §2: rejected → episode action_status = blocked + blockedReason
            let blocked_reason = result.message.as_deref();
            let _ = episodes_repo::update_action_status(
                app,
                episode_id,
                EpisodeActionStatus::Blocked,
                blocked_reason,
            );
            Ok(TradeIntentStatus::Rejected)
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // AcceptedPending 等 limit 订单 pipeline（Account spec §2 OrderStatus）接入
pub enum DispatchOutcome {
    /// 即时成交 / 调整成功 / 撤单成功 —— 无后续订单终态
    Executed {
        order_id: Option<String>,
        fill_ids: Vec<String>,
        position_id: Option<String>,
        account_event_ids: Vec<String>,
        message: Option<String>,
    },
    /// limit 进入 pending，等后续订单终态
    AcceptedPending {
        order_id: String,
        account_event_ids: Vec<String>,
    },
    /// Account 拒单（参数 / 规则 / 风控 / quote stale 等）
    Rejected {
        reason: String,
        message: Option<String>,
    },
}
