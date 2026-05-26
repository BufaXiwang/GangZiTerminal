//! Account canonical tools：`fetch_account` / `operate_account` / `update_watchlist`。
//!
//! 对齐 docs/design/agent-runtime-module.md §4 + account-module.md §4。
//! `operate_account` 7 action 全部通过 `pipeline::account::canonical::dispatch`
//! 落到 `account_orders` + AccountService。

use crate::domain::agent::types::ToolResultContent;
use crate::pipeline::agent::tools::{err_text, ok_json, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::AppHandle;

// ============ FetchAccountTool =========================================

pub struct FetchAccountTool {
    app: AppHandle,
}

impl FetchAccountTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for FetchAccountTool {
    fn name(&self) -> &'static str {
        "fetch_account"
    }

    fn description(&self) -> &'static str {
        "读取模拟账户：snapshot / 仓位 / 订单 / 自选 / 事件 / 触发。\
         缺行情时返回 quote_missing / data_partial warning。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "include": {"type": "object"},
                "positionStatus": {"type": "string", "enum": ["open", "closed", "all"]},
                "orderActive": {"type": "boolean"},
                "orderStatusIn": {"type": "array", "items": {"type": "string"}},
                "triggerHandled": {},
                "limit": {"type": "integer"},
                "offset": {"type": "integer"}
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        // 复用 adapter canonical fetch_account 实现，避免双份逻辑。
        use crate::adapters::account_canonical::{
            fetch_account as canonical_fetch, FetchAccountInclude, FetchAccountRequest,
        };
        let request = FetchAccountRequest {
            include: input
                .get("include")
                .and_then(|v| serde_json::from_value::<FetchAccountInclude>(v.clone()).ok()),
            position_status: input
                .get("positionStatus")
                .and_then(Value::as_str)
                .map(String::from),
            order_active: input.get("orderActive").and_then(Value::as_bool),
            order_status_in: input.get("orderStatusIn").and_then(Value::as_array).map(
                |arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                },
            ),
            trigger_handled: input.get("triggerHandled").cloned(),
            limit: input.get("limit").and_then(Value::as_i64),
            offset: input.get("offset").and_then(Value::as_i64),
        };
        let app = self.app.clone();
        match canonical_fetch(app, Some(request)).await {
            Ok(resp) => (
                ok_json(serde_json::to_value(resp).unwrap_or(Value::Null)),
                false,
            ),
            Err(e) => err_text(format!("fetch_account 失败：{e}")),
        }
    }
}

// ============ OperateAccountTool =======================================

pub struct OperateAccountTool {
    app: AppHandle,
}

impl OperateAccountTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for OperateAccountTool {
    fn name(&self) -> &'static str {
        "operate_account"
    }
    fn side_effect(&self) -> crate::pipeline::agent::tools::SideEffect {
        crate::pipeline::agent::tools::SideEffect::TradingWrite
    }
    fn timeout_ms(&self) -> u64 {
        60_000
    }

    fn description(&self) -> &'static str {
        "模拟账户写操作。action ∈ {place_order, cancel_order, open_position, \
         scale_position, close_position, adjust_protection, record_invalidation_signal}。\
         必须先 record_decision_episode 关联 episodeId（actionStatus = intended）。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "episodeId":   {"type": "string"},
                "accountInput": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": [
                                "place_order","cancel_order","open_position",
                                "scale_position","close_position",
                                "adjust_protection","record_invalidation_signal"
                            ]
                        }
                    },
                    "required": ["action"]
                }
            },
            "required": ["episodeId", "accountInput"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let episode_id = match input.get("episodeId").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return err_text("invalid_input: episodeId 必填（先调 record_decision_episode）"),
        };
        let acc = match input.get("accountInput") {
            Some(v) => v.clone(),
            None => return err_text("invalid_input: 缺 accountInput"),
        };
        let reason = acc
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        // spec §4：episodeId 必须指向同一 run 中已接受的 DecisionEpisode，
        // 且 actionStatus ∈ {intended, submitted}。fail closed，不调 Account。
        let app_c = self.app.clone();
        let ep_c = episode_id.clone();
        let run_c = ctx.run_id.clone();
        let validate_r = tokio::task::spawn_blocking(move || {
            crate::infrastructure::agent_runtime::episodes_repo::validate_for_operate(
                &app_c, &ep_c, &run_c,
            )
        })
        .await
        .map_err(|e| format!("validate 任务异常：{e}"));
        match validate_r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return err_text(e),
            Err(e) => return err_text(e),
        }

        // 1. TradeIntent(proposed → submitted)  + 立即推进 episode → submitted。
        // 把真实 tool_call_id 持久化到 trade_intents（spec §2 「toolCallId 是 Runtime /
        // Infra 审计关联键」），保证启动恢复时能查到 agent_tool_calls.output_payload_json。
        let intent_id = match crate::pipeline::agent_runtime::decisions::open_intent(
            &self.app,
            &ctx.run_id,
            &episode_id,
            ctx.tool_call_id.as_deref(),
            acc.clone(),
            reason,
        ) {
            Ok(id) => id,
            Err(msg) => return err_text(format!("db_error: {msg}")),
        };

        // 2. 走 canonical dispatch shim（pipeline/account/canonical）
        let result =
            crate::pipeline::account::canonical::dispatch(&self.app, &acc, &episode_id).await;

        // 3. 把 canonical result 映射成 DispatchOutcome 推进 TradeIntent。
        // spec §2 状态机：
        // - Account 返回 `accepted=true` 且 limit 还未成交 → AcceptedPending
        //   （有 orderId，无 fillIds / positionId）
        // - 其它 accepted → Executed
        let action_str = acc.get("action").and_then(Value::as_str).unwrap_or("");
        let order_type = acc.get("orderType").and_then(Value::as_str);
        // 白名单：spec §2 AcceptedPending 只发生在创建 limit 订单的 action
        let action_creates_order = matches!(
            action_str,
            "place_order" | "open_position" | "scale_position" | "close_position"
        );
        let is_limit_pending = result.accepted
            && action_creates_order
            && order_type == Some("limit")
            && result.fill_ids.is_empty()
            && result.position_id.is_none()
            && result.order_id.is_some();
        let outcome = if result.accepted && is_limit_pending {
            crate::pipeline::agent_runtime::decisions::DispatchOutcome::AcceptedPending {
                order_id: result.order_id.clone().unwrap_or_default(),
                account_event_ids: result.account_event_ids.clone(),
            }
        } else if result.accepted {
            crate::pipeline::agent_runtime::decisions::DispatchOutcome::Executed {
                order_id: result.order_id.clone(),
                fill_ids: result.fill_ids.clone(),
                position_id: result.position_id.clone(),
                account_event_ids: result.account_event_ids.clone(),
                message: result.message.clone(),
            }
        } else {
            crate::pipeline::agent_runtime::decisions::DispatchOutcome::Rejected {
                reason: result
                    .reason
                    .map(|c| c.as_str().to_string())
                    .unwrap_or_else(|| "unknown".into()),
                message: result.message.clone(),
            }
        };
        let _ = crate::pipeline::agent_runtime::decisions::mark_dispatch_outcome(
            &self.app,
            &intent_id,
            &episode_id,
            &ctx.run_id,
            ctx.tool_call_id.as_deref(),
            outcome,
        );

        // 4. 拼 tool 输出（spec agent-runtime-module.md §4 OperateAccountToolOutput）
        // accepted / reason / message / orderId / fillIds / positionId / triggerId /
        // rejectionEventId / accountEventIds(必填空数组也要出现) / snapshot(PacketAccountSnapshot,
        // **required，严格 spec 字段集**) / warnings。
        // spec §4: snapshot required —— svc.snapshot 失败也必须返回 fail closed empty stub。
        // 替换 dispatch 内部产出的 raw snapshot 为严格的 PacketAccountSnapshot 投影。
        let svc_snapshot = crate::pipeline::account::AccountService::new(self.app.clone())
            .snapshot();
        let snapshot_value = match &svc_snapshot {
            Ok(s) => crate::adapters::account_canonical::snapshot_to_packet_value(&self.app, s),
            Err(e) => {
                tracing::warn!(error = %e, "tool snapshot 失败，返回 fail-closed empty PacketAccountSnapshot");
                json!({
                    "capturedAt": chrono::Utc::now().to_rfc3339(),
                    "cash": 0.0,
                    "availableCash": 0.0,
                    "frozenCash": 0.0,
                    "marketValue": 0.0,
                    "totalAssets": 0.0,
                    "realizedPnl": 0.0,
                    "unrealizedPnl": 0.0,
                    "totalPnl": 0.0,
                    "pricedPositionCount": 0,
                    "unpricedPositionCount": 0,
                    "valuationFreshness": {"status": "missing"},
                    "openPositionCount": 0,
                    "pendingOrderCount": 0,
                    "warnings": ["data_partial"],
                })
            }
        };
        let mut value = serde_json::to_value(&result).unwrap_or(Value::Null);
        if let Some(obj) = value.as_object_mut() {
            obj.insert("snapshot".into(), snapshot_value);
        }
        let _ = episode_id;
        // spec §2：operate_account 必须持久化结构化 output_payload_json 到当前
        // tool_call_id 对应的 agent_tool_calls 行；recover_submitted 通过
        // trade_intents.tool_call_id 反查它做 TradeIntent 状态恢复。
        if let Some(tcid) = ctx.tool_call_id.as_deref() {
            let _ = crate::infrastructure::agent::tool_calls_repo::set_output_payload(
                &self.app,
                tcid,
                &value.to_string(),
            );
        }
        let is_error = !result.accepted;
        (ok_json(value), is_error)
    }
}
// ============ UpdateWatchlistTool ======================================

pub struct UpdateWatchlistTool {
    app: AppHandle,
}

impl UpdateWatchlistTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for UpdateWatchlistTool {
    fn name(&self) -> &'static str {
        "update_watchlist"
    }
    fn side_effect(&self) -> crate::pipeline::agent::tools::SideEffect {
        crate::pipeline::agent::tools::SideEffect::NonTradingWrite
    }

    fn description(&self) -> &'static str {
        "维护自选：add / remove / update_note。允许 user/agent/system 任一 actor。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "episodeId": {"type": "string"},
                "accountInput": {
                    "type": "object",
                    "properties": {
                        "action":  {"type": "string", "enum": ["add", "remove", "update_note"]},
                        "tsCode":  {"type": "string"},
                        "note":    {"type": "string"},
                        "reason":  {"type": "string"}
                    },
                    "required": ["action", "tsCode"]
                }
            },
            "required": ["accountInput"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        use crate::pipeline::account::{
            parse_watchlist_action, update_watchlist_dispatch, UpdateWatchlistInput,
            WatchlistAction,
        };
        let episode_id = input
            .get("episodeId")
            .and_then(Value::as_str)
            .map(String::from);
        let acc = match input.get("accountInput") {
            Some(v) => v.clone(),
            None => return err_text("invalid_input: missing accountInput"),
        };
        let action_str = acc.get("action").and_then(Value::as_str).unwrap_or("");
        let Some(action) = parse_watchlist_action(action_str) else {
            return err_text(format!("invalid_input: unknown action `{action_str}`"));
        };
        let ts_code = acc
            .get("tsCode")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if ts_code.is_empty() {
            return err_text("invalid_input: tsCode 为空");
        }
        // spec §4：update_watchlist.episodeId 若存在，必须指向同一 run 的 DecisionEpisode，
        // 且 action 一致性：accountInput.action = add → episode.action = add_watchlist；
        // remove → remove_watchlist。
        if let Some(ep) = &episode_id {
            // spec §4: episodeId 若存在必须指向同一 run 的 DecisionEpisode +
            // action 一致性。这里查 (run_id, action) 一次，两条规则都验。
            let app_c = self.app.clone();
            let ep_c = ep.clone();
            let run_c = ctx.run_id.clone();
            let row: Option<(String, String)> = tokio::task::spawn_blocking(move || {
                use crate::infrastructure::db::{migrate, open_database};
                let c = open_database(&app_c).ok()?;
                migrate(&c).ok()?;
                c.query_row(
                    "select run_id, action from decision_episodes where episode_id = ?1",
                    rusqlite::params![ep_c],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .ok()
            })
            .await
            .ok()
            .flatten();
            match row {
                None => {
                    return err_text(format!("not_found: episode {ep} 不存在"));
                }
                Some((ep_run, _)) if ep_run != run_c => {
                    return err_text(format!(
                        "invalid_input: episode {ep} 属于 run {ep_run}，与当前 run {run_c} 不一致"
                    ));
                }
                Some((_, ep_action)) => {
                    let expected = match action {
                        WatchlistAction::Add => Some("add_watchlist"),
                        WatchlistAction::Remove => Some("remove_watchlist"),
                        WatchlistAction::UpdateNote => None,
                    };
                    if let Some(exp) = expected {
                        if ep_action != exp {
                            return err_text(format!(
                                "invalid_input: episode action `{ep_action}` 与 watchlist 动作 `{}` 不匹配",
                                action_str
                            ));
                        }
                    }
                }
            }
        }

        let resp = update_watchlist_dispatch(
            &self.app,
            "agent",
            UpdateWatchlistInput {
                action,
                ts_code: ts_code.clone(),
                note: acc.get("note").and_then(Value::as_str).map(String::from),
                reason: acc.get("reason").and_then(Value::as_str).map(String::from),
            },
        );
        let mut value = serde_json::to_value(&resp).unwrap_or(Value::Null);
        if let (Some(ep), Some(obj)) = (episode_id, value.as_object_mut()) {
            obj.insert("episodeId".into(), Value::String(ep));
        }
        let is_error = !resp.accepted;
        (ok_json(value), is_error)
    }
}
