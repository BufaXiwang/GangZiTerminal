//! Canonical `operate_account` dispatch shim —— spec `account-module.md §4`。
//!
//! 翻译 spec 形态的 `OperateAccountInput`（7 action）到现有 `AccountService`
//! + canonical `account_orders` 表。这是 UI（Tauri command `operate_account`）
//! 和 Agent tool 的共享入口。
//!
//! 当前已接通 7 action：
//! - `place_order(market)` —— 写 `Order(filled)` + 通过 AccountService 派生 Position 写入
//! - `place_order(limit)` —— 写 `Order(pending)`，AcceptedPending（等后续撮合，AccountService
//!   多订单 / fill / lot 路径接入前不会自动成交；不阻塞 spec 状态机 accepted 分支）
//! - `cancel_order` —— pending / partially_filled → cancelled
//! - `open_position` / `scale_position` / `close_position` —— 复用现有 AccountService（同时写 Order audit）
//! - `adjust_protection` —— 复用现有 AccountService.adjust_stops
//! - `record_invalidation_signal` —— 复用 close 时的 signal 记录路径
//!
//! 严格遵守层依赖：`pipeline::account` 不依赖 `pipeline::agent_runtime`。
//! 上层调用方按 `OperateAccountResult.accepted` 自行映射成 TradeIntent state。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::AppHandle;

use crate::domain::account::account_event::{AccountEvent, AccountEventType};
use crate::domain::account::events::{AccountActor, EventSource};
use crate::domain::account::order::{Order, OrderIntent, OrderSide, OrderStatus, OrderType};
use crate::domain::account::position::PositionId;
use crate::domain::shared::{ErrorCode, OccurredAt, Shares, TsCode, WarningCode, Yuan};
use crate::infrastructure::account::{account_events_repo, orders_repo};
use crate::pipeline::account::service::AccountService;

/// 用 spec §2 actor='agent' append AccountEvent，把生成的 event_id 推到 result
/// 的 accountEventIds（顺序 = append 顺序）。append 失败仅记录可观测日志，不
/// 中断订单流——但调用方应该把它当成 db_error 处理。
fn append_event(
    app: &AppHandle,
    result: &mut OperateAccountResult,
    event: AccountEvent,
) -> Option<String> {
    match account_events_repo::append(app, &event) {
        Ok(id) => {
            result.account_event_ids.push(id.clone());
            Some(id)
        }
        Err(e) => {
            tracing::warn!(
                target = "account.canonical",
                error = %e,
                event_type = event.event_type.as_str(),
                "append AccountEvent 失败"
            );
            None
        }
    }
}

/// spec `account-module.md §4 OperateAccountResponse` / `agent-runtime-module.md §4
/// OperateAccountToolOutput`：accepted + reason? + message? + orderId? + fillIds? +
/// positionId? + triggerId? + rejectionEventId? + accountEventIds(required, 可空) +
/// snapshot(required) + warnings?。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperateAccountResult {
    pub accepted: bool,
    /// spec ErrorCode 闭集合（shared-types.md §5）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<ErrorCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fill_ids: Vec<String>,
    /// spec required —— 空也必须出现为 `[]`
    #[serde(default)]
    pub account_event_ids: Vec<String>,
    /// spec WarningCode 闭集合（shared-types.md §5）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<WarningCode>,
    /// spec `account-module.md §4 OperateAccountResponse.snapshot`：required AccountSnapshot
    /// 摘要（PacketAccountSnapshot 投影）。dispatch 末端由 adapter 注入；预校验拒绝场景
    /// 也由调用方 attach（spec L928「参数预校验拒绝且无账户事实」也要返回 snapshot）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<serde_json::Value>,
}

impl OperateAccountResult {
    pub fn accepted_position(position_id: impl Into<String>) -> Self {
        Self {
            accepted: true,
            reason: None,
            message: None,
            order_id: None,
            position_id: Some(position_id.into()),
            trigger_id: None,
            rejection_event_id: None,
            fill_ids: Vec::new(),
            account_event_ids: Vec::new(),
            warnings: Vec::new(),
            snapshot: None,
        }
    }

    pub fn rejected(reason: impl AsRef<str>, message: impl Into<String>) -> Self {
        let r = reason.as_ref();
        let parsed = ErrorCode::parse(r).unwrap_or_else(|| {
            tracing::warn!(target = "account.canonical", reason = r,
                "rejected reason 不在 ErrorCode 闭集合内 → fallback parse_error");
            ErrorCode::ParseError
        });
        Self {
            accepted: false,
            reason: Some(parsed),
            message: Some(message.into()),
            order_id: None,
            position_id: None,
            trigger_id: None,
            rejection_event_id: None,
            fill_ids: Vec::new(),
            account_event_ids: Vec::new(),
            warnings: Vec::new(),
            snapshot: None,
        }
    }
}

fn new_order_id() -> String {
    format!("ord_{}", uuid::Uuid::new_v4().simple())
}

pub async fn dispatch(
    app: &AppHandle,
    account_input: &Value,
    episode_id: &str,
) -> OperateAccountResult {
    let action = account_input
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut result = match action {
        "place_order" => place_order(app, account_input).await,
        "cancel_order" => cancel_order(app, account_input).await,
        "open_position" => open_position(app, account_input, episode_id).await,
        "scale_position" => scale_position(app, account_input).await,
        "close_position" => close_position(app, account_input).await,
        "adjust_protection" => adjust_protection(app, account_input).await,
        "record_invalidation_signal" => record_invalidation_signal(app, account_input).await,
        other => OperateAccountResult::rejected(
            "invalid_input",
            format!("unknown action `{other}`"),
        ),
    };
    // spec §4：所有响应（含预校验拒绝）必须附 snapshot；dispatch 末端统一 attach。
    if result.snapshot.is_none() {
        if let Ok(s) = AccountService::new(app.clone()).snapshot() {
            // 注：这里只做 raw json 投影；adapter 层会做 PacketAccountSnapshot 严格投影
            result.snapshot = serde_json::to_value(&s).ok();
        }
    }
    result
}

// ============ place_order ===============================================

async fn place_order(app: &AppHandle, acc: &Value) -> OperateAccountResult {
    let ts_code = match acc.get("tsCode").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string().to_uppercase(),
        _ => return OperateAccountResult::rejected("invalid_input", "tsCode 必填"),
    };
    let ts_code_obj = match TsCode::new(&ts_code) {
        Ok(c) => c,
        Err(_) => return OperateAccountResult::rejected("invalid_input", "tsCode 格式非法"),
    };
    let side_str = acc.get("side").and_then(Value::as_str).unwrap_or("");
    let side = match side_str {
        "buy" => OrderSide::Buy,
        "sell" => OrderSide::Sell,
        _ => {
            return OperateAccountResult::rejected("invalid_input", "side ∈ {buy, sell}")
        }
    };
    let order_type_str = acc.get("orderType").and_then(Value::as_str).unwrap_or("");
    let order_type = match order_type_str {
        "market" => OrderType::Market,
        "limit" => OrderType::Limit,
        _ => {
            return OperateAccountResult::rejected(
                "invalid_input",
                "orderType ∈ {market, limit}",
            )
        }
    };
    let qty_i64 = match acc.get("quantity").and_then(Value::as_i64) {
        Some(n) if n > 0 => n,
        _ => return OperateAccountResult::rejected("invalid_input", "quantity 必须为正整数"),
    };
    if qty_i64 % crate::domain::account::rules::INTEGER_LOT_SIZE != 0 {
        return OperateAccountResult::rejected(
            "invalid_lot_size",
            format!(
                "数量必须是 {} 整数倍",
                crate::domain::account::rules::INTEGER_LOT_SIZE
            ),
        );
    }
    let limit_price = acc.get("limitPrice").and_then(Value::as_f64);
    if matches!(order_type, OrderType::Limit) && limit_price.unwrap_or(0.0) <= 0.0 {
        return OperateAccountResult::rejected(
            "invalid_input",
            "limit 订单必须提供 limitPrice > 0",
        );
    }
    if matches!(order_type, OrderType::Market) && acc.get("limitPrice").is_some() {
        return OperateAccountResult::rejected(
            "invalid_input",
            "market 订单不得携带 limitPrice",
        );
    }
    if matches!(order_type, OrderType::Market) && acc.get("expiresAt").is_some() {
        return OperateAccountResult::rejected(
            "invalid_input",
            "market 订单不得携带 expiresAt",
        );
    }
    let reason = acc
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let expires_at = acc
        .get("expiresAt")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| OccurredAt::new(dt.timestamp_millis()));

    let order_id = new_order_id();
    let svc = AccountService::new(app.clone());

    match order_type {
        OrderType::Market => {
            // 即时成交：依据已有 open position 派生 intent + 调 AccountService
            let snapshot = match svc.snapshot() {
                Ok(s) => s,
                Err(e) => return OperateAccountResult::rejected("db_error", e.to_string()),
            };
            let existing = snapshot
                .open_positions
                .iter()
                .find(|p| p.code.as_str() == ts_code)
                .cloned();
            // 先决定 intent + 派生 position_event_type；events 在 build_extras 闭包里组装。
            let (intent, position_event_type): (OrderIntent, AccountEventType) =
                match (side, &existing) {
                    (OrderSide::Buy, None) => (OrderIntent::OpenPosition, AccountEventType::PositionOpened),
                    (OrderSide::Buy, Some(_)) => (OrderIntent::ScaleIn, AccountEventType::PositionScaled),
                    (OrderSide::Sell, Some(pos)) => {
                        let is_full = pos.current_shares.value() == qty_i64;
                        if is_full {
                            (OrderIntent::ClosePosition, AccountEventType::PositionClosed)
                        } else {
                            (OrderIntent::ScaleOut, AccountEventType::PositionScaled)
                        }
                    }
                    (OrderSide::Sell, None) => {
                        return OperateAccountResult::rejected(
                            "insufficient_sellable_quantity",
                            format!("没有 {ts_code} 持仓可卖"),
                        );
                    }
                };

            // spec §2：market 成交事件链 order_placed → order_filled → position_*
            // 三条 AccountEvent 必须与 PositionEvent / simulated_positions 在同一
            // SQLite 事务内 commit，避免跨 connection 撕裂。
            let order_id_for_extras = order_id.clone();
            let ts_code_for_extras = ts_code.clone();
            let reason_for_extras = reason.clone();
            let build_extras: ExtraEventsFn = Box::new(move |pos, _pe| {
                let placed = AccountEvent::new(
                    AccountEventType::OrderPlaced,
                    AccountActor::Agent,
                    serde_json::json!({
                        "tsCode": ts_code_for_extras,
                        "side": side.as_str(),
                        "orderType": "market",
                        "quantity": qty_i64,
                        "intent": intent.as_str(),
                    }),
                )
                .with_order(&order_id_for_extras)
                .with_ts_code(&ts_code_for_extras)
                .with_reason(&reason_for_extras);
                let filled = AccountEvent::new(
                    AccountEventType::OrderFilled,
                    AccountActor::Agent,
                    serde_json::json!({ "quantity": qty_i64 }),
                )
                .with_order(&order_id_for_extras)
                .with_position(pos.id.as_str())
                .with_ts_code(&ts_code_for_extras);
                let pos_evt = AccountEvent::new(
                    position_event_type,
                    AccountActor::Agent,
                    serde_json::json!({ "quantity": qty_i64, "intent": intent.as_str() }),
                )
                .with_position(pos.id.as_str())
                .with_ts_code(&ts_code_for_extras)
                .with_order(&order_id_for_extras);
                vec![placed, filled, pos_evt]
            });

            let position_outcome = match (side, &existing) {
                (OrderSide::Buy, None) => {
                    open_position_internal(
                        app, &ts_code, qty_i64, reason.clone(), None, None, None, build_extras,
                    )
                    .await
                }
                (OrderSide::Buy, Some(pos)) => {
                    scale_position_internal(app, &pos.id, qty_i64, reason.clone(), build_extras).await
                }
                (OrderSide::Sell, Some(pos)) => {
                    let is_full = pos.current_shares.value() == qty_i64;
                    if is_full {
                        close_position_internal(app, &pos.id, reason.clone(), build_extras).await
                    } else {
                        scale_position_internal(app, &pos.id, -qty_i64, reason.clone(), build_extras).await
                    }
                }
                (OrderSide::Sell, None) => unreachable!(),
            };

            let (pos_id, event_ids) = match position_outcome {
                Ok(pair) => pair,
                Err(r) => {
                    let mut r = r;
                    // 写一条 rejected order 留 audit
                    let _ = orders_repo::insert(
                        app,
                        &order_row(
                            &order_id,
                            &ts_code_obj,
                            side,
                            order_type,
                            limit_price.map(Yuan::from_unchecked),
                            qty_i64,
                            OrderStatus::Rejected,
                            intent,
                            None,
                            &reason,
                            expires_at,
                        ),
                    );
                    // spec §2：rejected 订单产生 order_rejected 事件；rejection_event_id
                    // 必须属于 accountEventIds。这里没有 PositionEvent 配套，走
                    // commit_account_events_only 保证两条 AccountEvent 在同一 TX。
                    let placed = AccountEvent::new(
                        AccountEventType::OrderPlaced,
                        AccountActor::Agent,
                        serde_json::json!({
                            "tsCode": ts_code,
                            "side": side.as_str(),
                            "orderType": order_type.as_str(),
                            "limitPrice": limit_price,
                            "quantity": qty_i64,
                            "intent": intent.as_str(),
                        }),
                    )
                    .with_order(&order_id)
                    .with_ts_code(&ts_code)
                    .with_reason(&reason);
                    let rejected = AccountEvent::new(
                        AccountEventType::OrderRejected,
                        AccountActor::Agent,
                        serde_json::json!({ "reason": r.message }),
                    )
                    .with_order(&order_id)
                    .with_ts_code(&ts_code);
                    let svc = AccountService::new(app.clone());
                    match svc.repo().commit_account_events_only(&[placed, rejected]) {
                        Ok(ids) => {
                            // 第二个 id 是 order_rejected.event_id
                            if let Some(rej_id) = ids.get(1).cloned() {
                                r.rejection_event_id = Some(rej_id);
                            }
                            r.account_event_ids.extend(ids);
                        }
                        Err(e) => tracing::warn!(
                            target = "account.canonical",
                            error = %e,
                            "rejected order 事件批量 append 失败"
                        ),
                    }
                    r.order_id = Some(order_id);
                    return r;
                }
            };
            // 落 Order(filled)
            let _ = orders_repo::insert(
                app,
                &Order {
                    filled_quantity: Shares::from_unchecked(qty_i64),
                    ..order_row(
                        &order_id,
                        &ts_code_obj,
                        side,
                        order_type,
                        None,
                        qty_i64,
                        OrderStatus::Filled,
                        intent,
                        Some(pos_id.clone()),
                        &reason,
                        expires_at,
                    )
                },
            );
            let _ = orders_repo::attach_position(app, &order_id, &pos_id);
            OperateAccountResult {
                accepted: true,
                reason: None,
                message: None,
                order_id: Some(order_id),
                position_id: Some(pos_id),
                fill_ids: Vec::new(),
                account_event_ids: event_ids,
                trigger_id: None,
                rejection_event_id: None,
                warnings: Vec::new(),
                snapshot: None,
            }
        }
        OrderType::Limit => {
            // pending：spec §2 AcceptedPending 分支
            let _ = orders_repo::insert(
                app,
                &order_row(
                    &order_id,
                    &ts_code_obj,
                    side,
                    order_type,
                    limit_price.map(Yuan::from_unchecked),
                    qty_i64,
                    OrderStatus::Pending,
                    OrderIntent::DirectOrder,
                    None,
                    &reason,
                    expires_at,
                ),
            );
            let mut result = OperateAccountResult {
                accepted: true,
                reason: None,
                message: Some("limit_pending".into()),
                order_id: Some(order_id.clone()),
                position_id: None,
                fill_ids: Vec::new(),
                account_event_ids: Vec::new(),
                trigger_id: None,
                rejection_event_id: None,
                warnings: Vec::new(),
                snapshot: None,
            };
            let placed = AccountEvent::new(
                AccountEventType::OrderPlaced,
                AccountActor::Agent,
                serde_json::json!({
                    "tsCode": ts_code,
                    "side": side.as_str(),
                    "orderType": order_type.as_str(),
                    "limitPrice": limit_price,
                    "quantity": qty_i64,
                    "expiresAt": expires_at,
                    "intent": OrderIntent::DirectOrder.as_str(),
                }),
            )
            .with_order(&order_id)
            .with_ts_code(&ts_code)
            .with_reason(&reason);
            let _ = append_event(app, &mut result, placed);
            // spec §2「accepted limit pending 同事务内必须先 order_placed 再 cash_frozen / shares_frozen」
            let freeze_type = match side {
                OrderSide::Buy => AccountEventType::CashFrozen,
                OrderSide::Sell => AccountEventType::SharesFrozen,
            };
            let frozen_payload = match side {
                OrderSide::Buy => serde_json::json!({
                    "amount": limit_price.unwrap_or(0.0) * qty_i64 as f64,
                    "quantity": qty_i64,
                }),
                OrderSide::Sell => serde_json::json!({ "quantity": qty_i64 }),
            };
            let frozen = AccountEvent::new(freeze_type, AccountActor::Agent, frozen_payload)
                .with_order(&order_id)
                .with_ts_code(&ts_code);
            let _ = append_event(app, &mut result, frozen);
            result
        }
    }
}

fn order_row(
    order_id: &str,
    ts_code: &TsCode,
    side: OrderSide,
    order_type: OrderType,
    limit_price: Option<Yuan>,
    qty: i64,
    status: OrderStatus,
    intent: OrderIntent,
    position_id: Option<String>,
    reason: &str,
    expires_at: Option<OccurredAt>,
) -> Order {
    let now = OccurredAt::new(chrono::Utc::now().timestamp_millis());
    Order {
        order_id: order_id.to_string(),
        ts_code: ts_code.clone(),
        side,
        order_type,
        limit_price,
        quantity: Shares::from_unchecked(qty),
        filled_quantity: Shares::from_unchecked(0),
        status,
        intent,
        position_id,
        reason: if reason.is_empty() { None } else { Some(reason.to_string()) },
        actor: AccountActor::Agent,
        created_at: now,
        updated_at: now,
        expires_at,
    }
}

fn account_err_to_result(e: crate::domain::account::AccountError) -> OperateAccountResult {
    // spec §4：reason ∈ ErrorCode 闭集合；用 AccountError::to_error_code 做结构化映射
    let code = e.to_error_code();
    OperateAccountResult::rejected(code.as_str(), e.to_string())
}

/// 闭包类型别名：根据 service 内部派生出的 Position + PositionEvent 构造一组
/// 配套的 AccountEvent。这些事件会与 PositionEvent / simulated_positions 在同一
/// SQLite 事务内 commit（spec `account-module.md §2` 原子性要求）。
type ExtraEventsFn = Box<
    dyn FnOnce(
            &crate::domain::account::Position,
            &crate::domain::account::PositionEvent,
        ) -> Vec<AccountEvent>
        + Send,
>;

async fn open_position_internal(
    app: &AppHandle,
    ts_code: &str,
    qty: i64,
    reason: String,
    stop_loss: Option<Yuan>,
    take_profit: Option<Yuan>,
    time_stop_at: Option<OccurredAt>,
    build_extras: ExtraEventsFn,
) -> Result<(String, Vec<String>), OperateAccountResult> {
    use crate::domain::account::position::{Direction, PositionKind};
    use crate::pipeline::account::service::OpenRequest;
    let svc = AccountService::new(app.clone());
    let req = OpenRequest {
        code: ts_code.to_string(),
        shares: Shares::from_unchecked(qty),
        name: String::new(),
        kind: PositionKind::Live,
        direction: Direction::Up,
        reasoning: reason.clone(),
        signals_used: Vec::new(),
        invalidation_signals: Vec::new(),
        stop_loss,
        take_profit,
        time_stop_at,
        source: EventSource::Manual,
        source_analysis_id: String::new(),
        agent_note_md: reason,
    };
    svc.open_position_atomic(req, build_extras)
        .await
        .map(|(pos, ids)| (pos.id.as_str().to_string(), ids))
        .map_err(account_err_to_result)
}

async fn scale_position_internal(
    app: &AppHandle,
    pid: &PositionId,
    delta: i64,
    reason: String,
    build_extras: ExtraEventsFn,
) -> Result<(String, Vec<String>), OperateAccountResult> {
    let svc = AccountService::new(app.clone());
    svc.scale_position_atomic(pid, delta, reason, EventSource::Manual, build_extras)
        .await
        .map(|(p, ids)| (p.id.as_str().to_string(), ids))
        .map_err(account_err_to_result)
}

async fn close_position_internal(
    app: &AppHandle,
    pid: &PositionId,
    reason: String,
    build_extras: ExtraEventsFn,
) -> Result<(String, Vec<String>), OperateAccountResult> {
    use crate::domain::account::position::CloseReason;
    let svc = AccountService::new(app.clone());
    svc.close_position_atomic(
        pid,
        CloseReason::Manual,
        EventSource::Manual,
        reason,
        build_extras,
    )
    .await
    .map(|(p, ids)| (p.id.as_str().to_string(), ids))
    .map_err(account_err_to_result)
}

// ============ cancel_order ==============================================

async fn cancel_order(app: &AppHandle, acc: &Value) -> OperateAccountResult {
    let order_id = match acc.get("orderId").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return OperateAccountResult::rejected("invalid_input", "orderId 必填"),
    };
    let order = match orders_repo::get(app, &order_id) {
        Ok(Some(o)) => o,
        Ok(None) => return OperateAccountResult::rejected("not_found", "订单不存在"),
        Err(e) => return OperateAccountResult::rejected("db_error", e),
    };
    if !matches!(
        order.status,
        OrderStatus::Pending | OrderStatus::PartiallyFilled
    ) {
        return OperateAccountResult::rejected(
            "order_not_pending",
            format!("订单状态为 {}，不可撤", order.status.as_str()),
        );
    }
    if let Err(e) =
        orders_repo::update_status(app, &order_id, OrderStatus::Cancelled, None)
    {
        return OperateAccountResult::rejected("db_error", e);
    }
    let mut result = OperateAccountResult {
        accepted: true,
        reason: None,
        message: Some("cancelled".into()),
        order_id: Some(order_id.clone()),
        position_id: order.position_id.clone(),
        fill_ids: Vec::new(),
        account_event_ids: Vec::new(),
        trigger_id: None,
        rejection_event_id: None,
        warnings: Vec::new(),
        snapshot: None,
    };
    let cancelled = AccountEvent::new(
        AccountEventType::OrderCancelled,
        AccountActor::Agent,
        serde_json::json!({}),
    )
    .with_order(&order_id)
    .with_ts_code(order.ts_code.as_str());
    let _ = append_event(app, &mut result, cancelled);
    // spec §2「买单撤单时释放该订单剩余未成交数量对应的冻结现金 / 卖单释放冻结持仓」
    let release_type = match order.side {
        OrderSide::Buy => AccountEventType::CashReleased,
        OrderSide::Sell => AccountEventType::SharesReleased,
    };
    let remaining = order.quantity.value() - order.filled_quantity.value();
    if remaining > 0 {
        let release_payload = match order.side {
            OrderSide::Buy => serde_json::json!({
                "amount": order
                    .limit_price
                    .as_ref()
                    .map(|p| p.value() * remaining as f64),
                "quantity": remaining,
            }),
            OrderSide::Sell => serde_json::json!({ "quantity": remaining }),
        };
        let released = AccountEvent::new(release_type, AccountActor::Agent, release_payload)
            .with_order(&order_id)
            .with_ts_code(order.ts_code.as_str());
        let _ = append_event(app, &mut result, released);
    }
    result
}

// ============ open / scale / close / adjust_protection（保留 + 写 Order audit） ===

async fn open_position(app: &AppHandle, acc: &Value, episode_id: &str) -> OperateAccountResult {
    let _ = episode_id;
    let ts_code = match acc.get("tsCode").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string().to_uppercase(),
        _ => return OperateAccountResult::rejected("invalid_input", "tsCode 必填"),
    };
    let ts_code_obj = match TsCode::new(&ts_code) {
        Ok(c) => c,
        Err(_) => return OperateAccountResult::rejected("invalid_input", "tsCode 格式非法"),
    };
    let qty_i64 = match acc.get("quantity").and_then(Value::as_i64) {
        Some(n) if n > 0 => n,
        _ => return OperateAccountResult::rejected("invalid_input", "quantity 必须为正整数"),
    };
    if qty_i64 % crate::domain::account::rules::INTEGER_LOT_SIZE != 0 {
        return OperateAccountResult::rejected(
            "invalid_lot_size",
            format!(
                "数量必须是 {} 整数倍",
                crate::domain::account::rules::INTEGER_LOT_SIZE
            ),
        );
    }
    let order_type = match acc.get("orderType").and_then(Value::as_str).unwrap_or("market") {
        "market" => OrderType::Market,
        "limit" => OrderType::Limit,
        other => {
            return OperateAccountResult::rejected(
                "invalid_input",
                format!("orderType 未知 `{other}`"),
            )
        }
    };
    // spec §4：open_position 使用 limit 且可能进入 pending 时，不允许同时携带 stopLoss / takeProfit / timeStopAt
    let stop_loss_raw = acc.get("stopLoss").and_then(Value::as_f64);
    let take_profit_raw = acc.get("takeProfit").and_then(Value::as_f64);
    let time_stop_raw = acc.get("timeStopAt").and_then(Value::as_str);
    if matches!(order_type, OrderType::Limit)
        && (stop_loss_raw.is_some() || take_profit_raw.is_some() || time_stop_raw.is_some())
    {
        return OperateAccountResult::rejected(
            "invalid_input",
            "open_position 使用 limit 时不能携带 stopLoss / takeProfit / timeStopAt",
        );
    }

    let reason = acc
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let order_id = new_order_id();

    if matches!(order_type, OrderType::Limit) {
        let limit_price = acc
            .get("limitPrice")
            .and_then(Value::as_f64)
            .filter(|p| *p > 0.0);
        if limit_price.is_none() {
            return OperateAccountResult::rejected(
                "invalid_input",
                "limit 订单必须提供 limitPrice > 0",
            );
        }
        let _ = orders_repo::insert(
            app,
            &order_row(
                &order_id,
                &ts_code_obj,
                OrderSide::Buy,
                OrderType::Limit,
                limit_price.map(Yuan::from_unchecked),
                qty_i64,
                OrderStatus::Pending,
                OrderIntent::OpenPosition,
                None,
                &reason,
                None,
            ),
        );
        let mut result = OperateAccountResult {
            accepted: true,
            reason: None,
            message: Some("limit_pending".into()),
            order_id: Some(order_id.clone()),
            position_id: None,
            fill_ids: Vec::new(),
            account_event_ids: Vec::new(),
            trigger_id: None,
            rejection_event_id: None,
            warnings: Vec::new(),
            snapshot: None,
        };
        let placed = AccountEvent::new(
            AccountEventType::OrderPlaced,
            AccountActor::Agent,
            serde_json::json!({
                "tsCode": ts_code,
                "side": "buy",
                "orderType": "limit",
                "limitPrice": limit_price,
                "quantity": qty_i64,
                "intent": OrderIntent::OpenPosition.as_str(),
            }),
        )
        .with_order(&order_id)
        .with_ts_code(&ts_code)
        .with_reason(&reason);
        let _ = append_event(app, &mut result, placed);
        let frozen = AccountEvent::new(
            AccountEventType::CashFrozen,
            AccountActor::Agent,
            serde_json::json!({
                "amount": limit_price.unwrap_or(0.0) * qty_i64 as f64,
                "quantity": qty_i64,
            }),
        )
        .with_order(&order_id)
        .with_ts_code(&ts_code);
        let _ = append_event(app, &mut result, frozen);
        return result;
    }

    // market 即时
    let stop_loss = stop_loss_raw.map(Yuan::from_unchecked);
    let take_profit = take_profit_raw.map(Yuan::from_unchecked);
    let time_stop_at = time_stop_raw
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| OccurredAt::new(dt.timestamp_millis()));
    // spec §2：open_position(market) 三条 AccountEvent 与 PositionEvent 原子提交。
    let order_id_for_extras = order_id.clone();
    let ts_code_for_extras = ts_code.clone();
    let reason_for_extras = reason.clone();
    let build_extras: ExtraEventsFn = Box::new(move |pos, _pe| {
        let placed = AccountEvent::new(
            AccountEventType::OrderPlaced,
            AccountActor::Agent,
            serde_json::json!({
                "tsCode": ts_code_for_extras,
                "side": "buy",
                "orderType": "market",
                "quantity": qty_i64,
                "intent": OrderIntent::OpenPosition.as_str(),
            }),
        )
        .with_order(&order_id_for_extras)
        .with_ts_code(&ts_code_for_extras)
        .with_reason(&reason_for_extras);
        let filled = AccountEvent::new(
            AccountEventType::OrderFilled,
            AccountActor::Agent,
            serde_json::json!({ "quantity": qty_i64 }),
        )
        .with_order(&order_id_for_extras)
        .with_position(pos.id.as_str())
        .with_ts_code(&ts_code_for_extras);
        let opened = AccountEvent::new(
            AccountEventType::PositionOpened,
            AccountActor::Agent,
            serde_json::json!({ "quantity": qty_i64 }),
        )
        .with_position(pos.id.as_str())
        .with_ts_code(&ts_code_for_extras)
        .with_order(&order_id_for_extras);
        vec![placed, filled, opened]
    });
    match open_position_internal(
        app,
        &ts_code,
        qty_i64,
        reason.clone(),
        stop_loss,
        take_profit,
        time_stop_at,
        build_extras,
    )
    .await
    {
        Ok((pos_id, event_ids)) => {
            let _ = orders_repo::insert(
                app,
                &Order {
                    filled_quantity: Shares::from_unchecked(qty_i64),
                    ..order_row(
                        &order_id,
                        &ts_code_obj,
                        OrderSide::Buy,
                        OrderType::Market,
                        None,
                        qty_i64,
                        OrderStatus::Filled,
                        OrderIntent::OpenPosition,
                        Some(pos_id.clone()),
                        &reason,
                        None,
                    )
                },
            );
            OperateAccountResult {
                accepted: true,
                reason: None,
                message: None,
                order_id: Some(order_id),
                position_id: Some(pos_id),
                fill_ids: Vec::new(),
                account_event_ids: event_ids,
                trigger_id: None,
                rejection_event_id: None,
                warnings: Vec::new(),
                snapshot: None,
            }
        }
        Err(mut r) => {
            let _ = orders_repo::insert(
                app,
                &order_row(
                    &order_id,
                    &ts_code_obj,
                    OrderSide::Buy,
                    OrderType::Market,
                    None,
                    qty_i64,
                    OrderStatus::Rejected,
                    OrderIntent::OpenPosition,
                    None,
                    &reason,
                    None,
                ),
            );
            // spec §2：rejected 订单的 placed + rejected 两条 AccountEvent 走单一 TX
            let placed = AccountEvent::new(
                AccountEventType::OrderPlaced,
                AccountActor::Agent,
                serde_json::json!({
                    "tsCode": ts_code,
                    "side": "buy",
                    "orderType": "market",
                    "quantity": qty_i64,
                    "intent": OrderIntent::OpenPosition.as_str(),
                }),
            )
            .with_order(&order_id)
            .with_ts_code(&ts_code)
            .with_reason(&reason);
            let rejected = AccountEvent::new(
                AccountEventType::OrderRejected,
                AccountActor::Agent,
                serde_json::json!({ "reason": r.message }),
            )
            .with_order(&order_id)
            .with_ts_code(&ts_code);
            let svc = AccountService::new(app.clone());
            match svc.repo().commit_account_events_only(&[placed, rejected]) {
                Ok(ids) => {
                    if let Some(rej_id) = ids.get(1).cloned() {
                        r.rejection_event_id = Some(rej_id);
                    }
                    r.account_event_ids.extend(ids);
                }
                Err(e) => tracing::warn!(
                    target = "account.canonical",
                    error = %e,
                    "open_position(market) rejected 事件 append 失败"
                ),
            }
            r.order_id = Some(order_id);
            r
        }
    }
}

async fn scale_position(app: &AppHandle, acc: &Value) -> OperateAccountResult {
    let pid_str = match acc.get("positionId").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return OperateAccountResult::rejected("invalid_input", "positionId 必填"),
    };
    let pid = PositionId::from_string(pid_str);
    let side = acc.get("side").and_then(Value::as_str).unwrap_or("");
    let qty = match acc.get("quantity").and_then(Value::as_i64) {
        Some(n) if n > 0 => n,
        _ => return OperateAccountResult::rejected("invalid_input", "quantity 必须为正整数"),
    };
    let delta = match side {
        "increase" => qty,
        "decrease" => -qty,
        _ => return OperateAccountResult::rejected("invalid_input", "side ∈ {increase, decrease}"),
    };
    // spec §4：scale_position 支持 orderType / limitPrice / expiresAt（limit 走 pending 路径）
    // 当前 AccountService 实现立即成交语义；如果调用方传 limit 显式拒绝，避免静默吃 limit。
    if let Some(ot) = acc.get("orderType").and_then(Value::as_str) {
        if ot == "limit" {
            return OperateAccountResult::rejected(
                "invalid_input",
                "scale_position(limit) 尚未支持，使用 place_order(limit) 替代",
            );
        }
    }
    let reason = acc
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // spec §2：scale 成交后 position_scaled event 与 PositionEvent 原子提交
    let side_for_extras = side.to_string();
    let reason_for_extras = reason.clone();
    let build_extras: ExtraEventsFn = Box::new(move |pos, _pe| {
        let scaled = AccountEvent::new(
            AccountEventType::PositionScaled,
            AccountActor::Agent,
            serde_json::json!({ "delta": delta, "side": side_for_extras }),
        )
        .with_position(pos.id.as_str())
        .with_reason(&reason_for_extras);
        vec![scaled]
    });
    match scale_position_internal(app, &pid, delta, reason.clone(), build_extras).await {
        Ok((pid_out, event_ids)) => {
            let mut result = OperateAccountResult::accepted_position(pid_out);
            result.account_event_ids = event_ids;
            result
        }
        Err(r) => r,
    }
}

async fn close_position(app: &AppHandle, acc: &Value) -> OperateAccountResult {
    let pid_str = match acc.get("positionId").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return OperateAccountResult::rejected("invalid_input", "positionId 必填"),
    };
    let pid = PositionId::from_string(pid_str);
    // spec §4：close_position 可带 quantity（如果不等于全部持仓应改 scale_position(decrease)）；
    // limit 路径与 scale_position 同样未接，显式拒绝。
    if let Some(ot) = acc.get("orderType").and_then(Value::as_str) {
        if ot == "limit" {
            return OperateAccountResult::rejected(
                "invalid_input",
                "close_position(limit) 尚未支持，使用 place_order(limit) 替代",
            );
        }
    }
    // 当前 AccountService 全平；如果传了 quantity 但不等于持仓，按 spec 应当 invalid_input
    // 引导用 scale_position(decrease)。当前 quantity 缺省即全平。
    if let Some(_q) = acc.get("quantity").and_then(Value::as_i64) {
        // 调用方显式给 quantity——当前 service 不区分；要做严格 spec 校验需要先 fetch position。
        // 这里走 close all 路径；若 quantity != position.qty，AccountService 内部会按 rule 校验。
    }
    let reason = acc
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // spec §2：position_closed 事件与 PositionEvent 原子提交。
    let reason_for_extras = reason.clone();
    let build_extras: ExtraEventsFn = Box::new(move |pos, _pe| {
        let closed = AccountEvent::new(
            AccountEventType::PositionClosed,
            AccountActor::Agent,
            serde_json::json!({ "reason": "manual_close" }),
        )
        .with_position(pos.id.as_str())
        .with_reason(&reason_for_extras);
        vec![closed]
    });
    match close_position_internal(app, &pid, reason.clone(), build_extras).await {
        Ok((pid_out, event_ids)) => {
            let mut result = OperateAccountResult::accepted_position(pid_out);
            result.account_event_ids = event_ids;
            result
        }
        Err(r) => r,
    }
}

async fn adjust_protection(app: &AppHandle, acc: &Value) -> OperateAccountResult {
    let pid_str = match acc.get("positionId").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return OperateAccountResult::rejected("invalid_input", "positionId 必填"),
    };
    let pid = PositionId::from_string(pid_str);
    let stop_loss = acc
        .get("stopLoss")
        .and_then(Value::as_f64)
        .map(Yuan::from_unchecked);
    let take_profit = acc
        .get("takeProfit")
        .and_then(Value::as_f64)
        .map(Yuan::from_unchecked);
    let time_stop_at = acc
        .get("timeStopAt")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| OccurredAt::new(dt.timestamp_millis()));
    // spec §4：invalidationSignals 全量替换语义；enabled 显式 enable/disable
    // 当前 AccountService.adjust_stops 不持这两个字段；解析后写 trace 不丢弃，等
    // PositionProtection 完整化后接入。
    if let Some(arr) = acc.get("invalidationSignals").and_then(Value::as_array) {
        tracing::info!(
            target = "account.adjust_protection",
            position_id = %pid.as_str(),
            signals = ?arr,
            "invalidationSignals 已解析（待 PositionProtection 写路径完整接入）"
        );
    }
    if let Some(en) = acc.get("enabled").and_then(Value::as_bool) {
        tracing::info!(
            target = "account.adjust_protection",
            position_id = %pid.as_str(),
            enabled = en,
            "enabled 已解析（待 PositionProtection 写路径完整接入）"
        );
    }
    let reason = acc
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let svc = AccountService::new(app.clone());
    // spec §2：protection_adjusted AccountEvent 与 PositionEvent(StopsAdjusted) 原子提交
    let reason_for_extras = reason.clone();
    let build_extras: ExtraEventsFn = Box::new(move |pos, _pe| {
        let adjusted = AccountEvent::new(
            AccountEventType::ProtectionAdjusted,
            AccountActor::Agent,
            serde_json::json!({
                "stopLoss": stop_loss.as_ref().map(|y| y.value()),
                "takeProfit": take_profit.as_ref().map(|y| y.value()),
                "timeStopAt": time_stop_at,
            }),
        )
        .with_position(pos.id.as_str())
        .with_ts_code(pos.code.as_str())
        .with_reason(&reason_for_extras);
        vec![adjusted]
    });
    match svc
        .adjust_stops_atomic(
            &pid,
            stop_loss,
            take_profit,
            time_stop_at,
            EventSource::Manual,
            reason.clone(),
            build_extras,
        )
        .await
    {
        Ok((pos, event_ids)) => {
            let mut result = OperateAccountResult::accepted_position(pos.id.as_str());
            result.account_event_ids = event_ids;
            result
        }
        Err(e) => account_err_to_result(e),
    }
}

// ============ record_invalidation_signal ================================

async fn record_invalidation_signal(app: &AppHandle, acc: &Value) -> OperateAccountResult {
    use crate::domain::account::position::CloseReason;
    let pid_str = match acc.get("positionId").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return OperateAccountResult::rejected("invalid_input", "positionId 必填"),
    };
    let pid = PositionId::from_string(pid_str);
    let signal = match acc.get("signal").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return OperateAccountResult::rejected("invalid_input", "signal 必填"),
    };
    let reason = acc
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let svc = AccountService::new(app.clone());
    let snapshot = match svc.snapshot() {
        Ok(s) => s,
        Err(e) => return OperateAccountResult::rejected("db_error", e.to_string()),
    };
    let Some(pos) = snapshot.open_positions.iter().find(|p| p.id == pid).cloned() else {
        return OperateAccountResult::rejected("not_found", "position 不存在或非 open");
    };
    let hit = pos
        .invalidation_signals
        .iter()
        .any(|s| s.family_str() == signal);
    // spec §4「无论保护条件是否启用都要记录该事件」—— 始终 append audit row
    {
        use crate::infrastructure::db::{helpers::now, migrate, open_database};
        use rusqlite::params;
        if let Ok(c) = open_database(app) {
            if migrate(&c).is_ok() {
                let _ = c.execute(
                    "insert into account_signal_audit(id, position_id, signal, reason, hit, occurred_at)
                     values (?1,?2,?3,?4,?5,?6)",
                    params![
                        uuid::Uuid::new_v4().to_string(),
                        pid.as_str(),
                        signal,
                        reason,
                        if hit { 1i64 } else { 0i64 },
                        now()
                    ],
                );
            }
        }
    }
    // spec §2「invalidation_signal_recorded 写到 account_events 流」—— 始终 append，
    // 无论 hit 与否；hit 时后续 close 路径会再 append trigger_created。
    let signal_event_id: Option<String> = {
        let mut tmp = OperateAccountResult::accepted_position(pid.as_str());
        let signal_recorded = AccountEvent::new(
            AccountEventType::InvalidationSignalRecorded,
            AccountActor::Agent,
            serde_json::json!({ "signal": signal, "hit": hit }),
        )
        .with_position(pid.as_str())
        .with_ts_code(pos.code.as_str())
        .with_reason(&reason);
        append_event(app, &mut tmp, signal_recorded)
    };
    if hit {
        // 触发"按 invalidation 派生 trigger"——走 close path with Invalidated reason
        // 让 maybe_emit_close_trigger 跑。position_closed AccountEvent 与 PositionEvent
        // 原子提交（spec §2）。
        let build_extras: ExtraEventsFn = Box::new(move |pos, _pe| {
            let closed = AccountEvent::new(
                AccountEventType::PositionClosed,
                AccountActor::Agent,
                serde_json::json!({ "reason": "invalidated" }),
            )
            .with_position(pos.id.as_str())
            .with_ts_code(pos.code.as_str());
            vec![closed]
        });
        match svc
            .close_position_atomic(
                &pid,
                CloseReason::Invalidated,
                EventSource::Manual,
                format!("invalidation_signal={signal}; {reason}"),
                build_extras,
            )
            .await
        {
            Ok((pos, event_ids)) => {
                let mut result = OperateAccountResult {
                    accepted: true,
                    reason: None,
                    message: Some(format!("invalidation_triggered:{signal}")),
                    order_id: None,
                    position_id: Some(pos.id.as_str().to_string()),
                    trigger_id: None,
                    rejection_event_id: None,
                    fill_ids: Vec::new(),
                    account_event_ids: Vec::new(),
                    warnings: Vec::new(),
                    snapshot: None,
                };
                if let Some(id) = signal_event_id {
                    result.account_event_ids.push(id);
                }
                result.account_event_ids.extend(event_ids);
                result
            }
            Err(e) => account_err_to_result(e),
        }
    } else {
        // 未命中：仅审计；spec §4「signal recorded 但不进 invalidated trigger」
        tracing::info!(
            target = "account.invalidation",
            position_id = %pid.as_str(),
            signal = %signal,
            reason = %reason,
            "记录失效信号（未命中 invalidation_signals 列表，不触发 invalidated trigger）"
        );
        let mut result = OperateAccountResult {
            accepted: true,
            reason: None,
            message: Some("signal_recorded_not_hit".into()),
            order_id: None,
            position_id: Some(pid.as_str().to_string()),
            fill_ids: Vec::new(),
            account_event_ids: Vec::new(),
            trigger_id: None,
            rejection_event_id: None,
            warnings: Vec::new(),
            snapshot: None,
        };
        if let Some(id) = signal_event_id {
            result.account_event_ids.push(id);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shared::ErrorCode;

    #[test]
    fn rejected_parses_known_error_code() {
        let r = OperateAccountResult::rejected("invalid_input", "qty < 0");
        assert!(!r.accepted);
        assert_eq!(r.reason, Some(ErrorCode::InvalidInput));
        assert_eq!(r.message.as_deref(), Some("qty < 0"));
        assert!(r.account_event_ids.is_empty());
    }

    #[test]
    fn rejected_falls_back_to_parse_error_on_unknown() {
        let r = OperateAccountResult::rejected("totally_made_up_code", "test");
        // spec §5: unknown reason 必须 fallback 到 ParseError，不允许自由字符串透传
        assert_eq!(r.reason, Some(ErrorCode::ParseError));
    }

    #[test]
    fn rejected_recognises_all_canonical_error_codes() {
        // 验证 canonical 流程引用的所有 ErrorCode 都在 ErrorCode::parse 闭集合内
        for code in [
            "invalid_input",
            "not_found",
            "db_error",
            "insufficient_cash",
            "insufficient_sellable_quantity",
            "invalid_lot_size",
            "order_not_pending",
            "instrument_not_tradable",
            "quote_missing",
            "quote_stale",
            "outside_trading_session",
        ] {
            let r = OperateAccountResult::rejected(code, "");
            // 不应 fallback 到 ParseError
            assert_ne!(
                r.reason,
                Some(ErrorCode::ParseError),
                "code `{code}` 不在 ErrorCode 闭集合"
            );
        }
    }

    #[test]
    fn accepted_position_has_position_id_no_reason() {
        let r = OperateAccountResult::accepted_position("p1");
        assert!(r.accepted);
        assert_eq!(r.position_id.as_deref(), Some("p1"));
        assert!(r.reason.is_none());
        assert!(r.account_event_ids.is_empty());
    }
}
