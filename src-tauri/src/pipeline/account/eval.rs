//! 触发评估流 — pending orders 成交评估 + 过期处理 + protection 评估。
//!
//! Spec: docs/design/account-module.md §3 触发评估流 / §5 模拟成交规则 / §5 保护条件触发
//!
//! 设计：
//! - 评估是分批的（spec §5：每 tick 最多 `batch_size` 条；返回 has_more / next_cursor）。
//! - 排序固定：先 pending orders（updated_at asc），再 open positions with protection（opened_at asc）。
//! - trigger_id 用 `TriggerKey` 稳定键；同一交易日 + revision + threshold 只生成一次。

use crate::domain::account::events::{AccountEvent, AccountEventType};
use crate::domain::account::money::{
    apply_buy_avg_cost, apply_sell_realized_pnl, compute_commission, compute_stamp_tax,
    compute_transfer_fee,
};
use crate::domain::account::requests::AccountActor;
use crate::domain::account::triggers::{AccountTrigger, AccountTriggerResult, AccountTriggerType, TriggerKey};
use crate::domain::account::types::{
    OrderSide, OrderStatus, OrderType, Position, PositionLot, PositionProtection,
    PositionStatus, TradeFill, TradingActor,
};
use crate::domain::shared::{
    resolve_market_time, FreshnessStatus, InstrumentCategory, Money, OccurredAt, Price, Shares,
    TsCode, WarningCode,
};
use crate::infrastructure::account::repository::{AccountRepository, FreezeEntry};
use crate::infrastructure::db::AppDb;
use crate::pipeline::account::fills::{simulate_limit, FillDecision, NotEligibleReason};
use crate::pipeline::account::quote_gateway::AccountQuoteGateway;
use crate::pipeline::account::service::{consume_lots_fifo, quote_err_to_warning};
use chrono::Utc;
use rust_decimal::Decimal;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

pub struct EvalDeps {
    pub db: AppDb,
    pub gateway: Arc<dyn AccountQuoteGateway>,
    pub fee_policy: crate::domain::account::policy::AccountFeePolicy,
}

/// 评估批次输入。
pub struct EvalInput<'a> {
    pub deps: &'a EvalDeps,
    pub now: OccurredAt,
    pub batch_size: usize,
    pub cursor: Option<String>,
}

/// Cursor 编码格式 — `phase/updated_at/trigger_key` 持久排序键。
///
/// Spec: account-module.md §2 触发事件模型:
///   `nextCursor` 必须基于持久排序键生成；它必须可跨进程重启后恢复同一批次之后的扫描位置。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EvalCursor {
    pub phase: EvalPhase,
    /// `updated_at` (orders) 或 `opened_at` (positions) 的 ISO-8601 字符串。
    pub anchor_ts: String,
    /// `order_id` 或 `position_id` — 同 anchor_ts 的稳定 tie-breaker。
    pub anchor_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EvalPhase {
    Orders,
    Positions,
}

impl EvalPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Orders => "orders",
            Self::Positions => "positions",
        }
    }
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "orders" => Some(Self::Orders),
            "positions" => Some(Self::Positions),
            _ => None,
        }
    }
}

impl EvalCursor {
    pub(crate) fn encode(&self) -> String {
        format!("eval/{}/{}/{}", self.phase.as_str(), self.anchor_ts, self.anchor_key)
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        // expected: eval/<phase>/<ts>/<key>
        let rest = s.strip_prefix("eval/")?;
        let parts: Vec<&str> = rest.splitn(3, '/').collect();
        if parts.len() != 3 {
            return None;
        }
        Some(Self {
            phase: EvalPhase::from_str(parts[0])?,
            anchor_ts: parts[1].to_string(),
            anchor_key: parts[2].to_string(),
        })
    }
}

/// 触发评估的同步实现（spec §4 evaluate_account_triggers）。
///
/// Spec: account-module.md §2 触发事件模型:
///   `nextCursor` 基于持久排序键 `(phase, updated_at/opened_at, id)`。
///   进程重启后调用方传入 cursor，从上一批结束位置之后继续扫描。
pub fn evaluate_account_triggers(input: EvalInput<'_>) -> AccountTriggerResult {
    let repo = AccountRepository::new(&input.deps.db);
    let mut all_triggers: Vec<AccountTrigger> = Vec::new();
    let mut all_event_ids: Vec<String> = Vec::new();
    let mut warnings: Vec<WarningCode> = Vec::new();
    let mut processed = 0usize;
    let mut has_more = false;
    let now = input.now;

    let cursor = input.cursor.as_deref().and_then(EvalCursor::parse);
    let start_phase = cursor.as_ref().map(|c| c.phase).unwrap_or(EvalPhase::Orders);
    // 同 anchor 之后才处理（lexical compare on (anchor_ts, anchor_key)）
    let resume_after: Option<(String, String)> =
        cursor.as_ref().map(|c| (c.anchor_ts.clone(), c.anchor_key.clone()));

    let mut last_processed: Option<(EvalPhase, String, String)> = None;

    // Phase 1: pending orders（按 updated_at asc, order_id asc）
    if matches!(start_phase, EvalPhase::Orders) {
        let active_orders = repo.list_active_orders().unwrap_or_default();
        let mut orders_sorted: Vec<_> = active_orders;
        orders_sorted
            .sort_by(|a, b| a.updated_at.cmp(&b.updated_at).then(a.order_id.cmp(&b.order_id)));
        for order in orders_sorted {
            let order_ts = order.updated_at.to_rfc3339();
            if let Some((rts, rkey)) = &resume_after {
                // strict-greater-than: skip until past resume anchor
                if (order_ts.as_str(), order.order_id.as_str()) <= (rts.as_str(), rkey.as_str()) {
                    continue;
                }
            }
            if processed >= input.batch_size {
                has_more = true;
                break;
            }
            // Expiration check
            if let Some(expiry) = order.expires_at {
                if now >= expiry {
                    if let Some((trig, ev_ids)) = expire_order(&repo, &order) {
                        all_event_ids.extend(ev_ids);
                        if let Some(t) = trig {
                            all_triggers.push(t);
                        }
                    }
                    processed += 1;
                    last_processed = Some((EvalPhase::Orders, order_ts, order.order_id.clone()));
                    continue;
                }
            }
            // Fill evaluation (limit only — market 已在 operate_account 中处理)
            if matches!(order.order_type, OrderType::Limit) {
                let ctx = resolve_market_time(now);
                let snap = input.deps.gateway.get_snapshot(&order.ts_code);
                match snap {
                    Ok(snapshot) => {
                        let limit_price = order.limit_price.expect("limit order has limit_price");
                        let remaining = Shares(order.quantity.0 - order.filled_quantity.0);
                        if remaining.0 <= 0 {
                            last_processed =
                                Some((EvalPhase::Orders, order_ts, order.order_id.clone()));
                            continue;
                        }
                        let decision = simulate_limit(
                            &snapshot,
                            order.side,
                            limit_price,
                            remaining,
                            ctx.is_trading_time,
                        );
                        match decision {
                            FillDecision::Filled(exec) | FillDecision::PartiallyFilled(exec) => {
                                let full_fill = exec.quantity.0 >= remaining.0;
                                if let Some((trig, ev_ids)) = commit_limit_fill(
                                    &repo,
                                    input.deps,
                                    &order,
                                    exec.price,
                                    exec.quantity,
                                    full_fill,
                                    now,
                                ) {
                                    all_event_ids.extend(ev_ids);
                                    if let Some(t) = trig {
                                        all_triggers.push(t);
                                    }
                                }
                            }
                            FillDecision::NotEligible(NotEligibleReason::QuoteMissing) => {
                                if !warnings.contains(&WarningCode::QuoteMissing) {
                                    warnings.push(WarningCode::QuoteMissing);
                                }
                            }
                            FillDecision::NotEligible(NotEligibleReason::QuoteStale) => {
                                // limit pending OK; do nothing
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        let w = quote_err_to_warning(e.kind);
                        if !warnings.contains(&w) {
                            warnings.push(w);
                        }
                    }
                }
            }
            processed += 1;
            last_processed = Some((EvalPhase::Orders, order_ts, order.order_id.clone()));
        }
    }

    // Phase 2: protection evaluation（按 opened_at asc, position_id asc）
    if !has_more {
        let positions_with_prot = repo
            .list_open_positions_with_protection()
            .unwrap_or_default();
        // Sort already done in SQL (opened_at ASC, position_id ASC).
        // Resume only applies when start_phase = Positions AND we are continuing this phase.
        let positions_resume = if matches!(start_phase, EvalPhase::Positions) {
            resume_after.clone()
        } else {
            None
        };
        for (pos, prot) in positions_with_prot {
            let pos_ts = pos.opened_at.to_rfc3339();
            if let Some((rts, rkey)) = &positions_resume {
                if (pos_ts.as_str(), pos.position_id.as_str()) <= (rts.as_str(), rkey.as_str()) {
                    continue;
                }
            }
            if processed >= input.batch_size {
                has_more = true;
                break;
            }
            evaluate_protection(
                &repo,
                input.deps,
                &pos,
                &prot,
                now,
                &mut all_triggers,
                &mut all_event_ids,
                &mut warnings,
            );
            processed += 1;
            last_processed = Some((EvalPhase::Positions, pos_ts, pos.position_id.clone()));
        }
    }

    let next_cursor = if has_more {
        last_processed.map(|(phase, anchor_ts, anchor_key)| {
            EvalCursor {
                phase,
                anchor_ts,
                anchor_key,
            }
            .encode()
        })
    } else {
        None
    };

    AccountTriggerResult {
        triggers: all_triggers,
        account_event_ids: all_event_ids,
        has_more,
        next_cursor,
        warnings,
    }
}

// ----------------------------------------------------------------------------
// expire pending order
// ----------------------------------------------------------------------------

fn expire_order(
    repo: &AccountRepository<'_>,
    order: &crate::domain::account::types::Order,
) -> Option<(Option<AccountTrigger>, Vec<String>)> {
    let mut event_ids = Vec::new();
    let mut trigger: Option<AccountTrigger> = None;
    let now = Utc::now();
    let mut updated_order = order.clone();
    updated_order.status = OrderStatus::Expired;
    updated_order.updated_at = now;
    let freeze = repo.get_freeze(&order.order_id).ok().flatten();
    let result: rusqlite::Result<()> = repo.tx(|tx| {
        AccountRepository::upsert_order(tx, &updated_order)?;
        let ev = AccountEvent {
            event_id: new_id("evt"),
            event_type: AccountEventType::OrderExpired,
            order_id: Some(order.order_id.clone()),
            fill_id: None,
            position_id: order.position_id.clone(),
            ts_code: Some(order.ts_code.clone()),
            reason: Some("expired".into()),
            actor: AccountActor::System.as_str().into(),
            payload: json!({}),
            occurred_at: now,
        };
        AccountRepository::append_event(tx, &ev)?;
        let expired_event_id = ev.event_id.clone();
        event_ids.push(ev.event_id);

        // 释放冻结
        if let Some(f) = &freeze {
            match f.side {
                OrderSide::Buy => {
                    if f.frozen_cash.0 > Decimal::ZERO {
                        let ev2 = AccountEvent {
                            event_id: new_id("evt"),
                            event_type: AccountEventType::CashReleased,
                            order_id: Some(order.order_id.clone()),
                            fill_id: None,
                            position_id: None,
                            ts_code: Some(order.ts_code.clone()),
                            reason: Some("expired".into()),
                            actor: AccountActor::System.as_str().into(),
                            payload: json!({ "amount": f.frozen_cash.0.to_string() }),
                            occurred_at: now,
                        };
                        AccountRepository::append_event(tx, &ev2)?;
                        event_ids.push(ev2.event_id);
                    }
                }
                OrderSide::Sell => {
                    if f.frozen_shares.0 > 0 {
                        let ev2 = AccountEvent {
                            event_id: new_id("evt"),
                            event_type: AccountEventType::SharesReleased,
                            order_id: Some(order.order_id.clone()),
                            fill_id: None,
                            position_id: order.position_id.clone(),
                            ts_code: Some(order.ts_code.clone()),
                            reason: Some("expired".into()),
                            actor: AccountActor::System.as_str().into(),
                            payload: json!({ "shares": f.frozen_shares.0 }),
                            occurred_at: now,
                        };
                        AccountRepository::append_event(tx, &ev2)?;
                        event_ids.push(ev2.event_id);
                        for fl in &f.frozen_lots {
                            let lots = AccountRepository::list_lots_by_position_conn(
                                tx,
                                order.position_id.as_deref().unwrap_or(""),
                            )?;
                            if let Some(lot) = lots.iter().find(|l| l.lot_id == fl.lot_id) {
                                let new_frozen =
                                    Shares((lot.frozen_quantity.0 - fl.quantity).max(0));
                                AccountRepository::update_lot_quantities(
                                    tx,
                                    &lot.lot_id,
                                    lot.remaining_quantity,
                                    new_frozen,
                                )?;
                            }
                        }
                    }
                }
            }
            AccountRepository::delete_freeze(tx, &order.order_id)?;
        }

        // Order-expired trigger
        let trig = AccountTrigger {
            trigger_id: TriggerKey::OrderTerminal {
                trigger_type: AccountTriggerType::OrderExpired,
                order_id: &order.order_id,
                ts_code: &order.ts_code,
                event_id: &expired_event_id,
            }
            .stable_id(),
            trigger_type: AccountTriggerType::OrderExpired,
            order_id: Some(order.order_id.clone()),
            position_id: order.position_id.clone(),
            ts_code: Some(order.ts_code.clone()),
            price: None,
            threshold: None,
            quote_freshness: None,
            warnings: vec![],
            event_id: expired_event_id,
            handled: false,
            occurred_at: now,
        };
        if AccountRepository::insert_trigger_if_new(tx, &trig)? {
            trigger = Some(trig);
        }
        Ok(())
    });
    if result.is_err() {
        return None;
    }
    Some((trigger, event_ids))
}

// ----------------------------------------------------------------------------
// commit limit fill (called by eval)
// ----------------------------------------------------------------------------

/// Spec: account-module.md §2 成交模型 / 仓位模型 — 现金 / PnL / lot cost 必须包含 transferFee。
fn commit_limit_fill(
    repo: &AccountRepository<'_>,
    deps: &EvalDeps,
    order: &crate::domain::account::types::Order,
    exec_price: Price,
    exec_quantity: Shares,
    full_fill: bool,
    now: OccurredAt,
) -> Option<(Option<AccountTrigger>, Vec<String>)> {
    let mut event_ids = Vec::new();
    let commission = compute_commission(exec_price, exec_quantity, &deps.fee_policy);
    let stamp_tax = if matches!(order.side, OrderSide::Sell) {
        compute_stamp_tax(exec_price, exec_quantity, &deps.fee_policy)
    } else {
        Money(Decimal::ZERO)
    };
    // Instrument category for transfer_fee — lookup via quotes facade.
    let category = crate::pipeline::quotes::facade::get_instrument(&deps.db, &order.ts_code)
        .ok()
        .flatten()
        .map(|i| i.category)
        .unwrap_or(InstrumentCategory::Stock);
    let transfer_fee =
        compute_transfer_fee(exec_price, exec_quantity, &deps.fee_policy, &order.ts_code, category);
    let order_id = order.order_id.clone();
    let mut trigger: Option<AccountTrigger> = None;

    let meta = match repo.get_meta() {
        Ok(Some(m)) => m,
        _ => return None,
    };

    // Find/derive position
    let existing = repo.find_open_position_by_ts_code(&order.ts_code).ok().flatten();
    let position_id = match (order.side, existing.as_ref(), order.position_id.as_ref()) {
        (OrderSide::Sell, _, Some(pid)) => pid.clone(),
        (_, Some(p), _) => p.position_id.clone(),
        (OrderSide::Buy, None, _) => format!("pos_{}", Uuid::new_v4().simple()),
        (OrderSide::Sell, None, _) => return None,
    };
    // Lookup instrument name for new position (spec §2 Position.name 来自标的元信息).
    let instrument_name = crate::pipeline::quotes::facade::get_instrument(&deps.db, &order.ts_code)
        .ok()
        .flatten()
        .map(|i| i.name)
        .unwrap_or_else(|| order.ts_code.as_str().to_string());
    let trade_amount = exec_price.0 * Decimal::from(exec_quantity.0);
    let cash_delta = match order.side {
        OrderSide::Buy => -trade_amount - commission.0 - transfer_fee.0,
        OrderSide::Sell => trade_amount - commission.0 - stamp_tax.0 - transfer_fee.0,
    };
    // Build position after — pass stamp_tax + transfer_fee for proper PnL / cost basis.
    let (position_event, position_after) = derive_position_after_fill(
        order.side,
        &existing,
        &order.ts_code,
        exec_price,
        exec_quantity,
        commission,
        stamp_tax,
        transfer_fee,
        now,
        position_id.clone(),
        instrument_name,
    );

    let mut updated_order = order.clone();
    updated_order.filled_quantity = Shares(order.filled_quantity.0 + exec_quantity.0);
    updated_order.status = if full_fill {
        OrderStatus::Filled
    } else {
        OrderStatus::PartiallyFilled
    };
    updated_order.position_id = Some(position_id.clone());
    updated_order.updated_at = now;

    let fill_id = new_id("fill");
    let fill = TradeFill {
        fill_id: fill_id.clone(),
        order_id: order_id.clone(),
        position_id: position_id.clone(),
        ts_code: order.ts_code.clone(),
        side: order.side,
        price: exec_price,
        quantity: exec_quantity,
        commission,
        stamp_tax,
        transfer_fee,
        occurred_at: now,
    };

    let freeze_before = repo.get_freeze(&order.order_id).ok().flatten();
    let order_filled_ev_id = new_id("evt");
    let result: rusqlite::Result<()> = repo.tx(|tx| {
        AccountRepository::upsert_order(tx, &updated_order)?;
        AccountRepository::insert_fill(tx, &fill)?;
        AccountRepository::upsert_position(tx, &position_after)?;

        // Position event
        let pos_ev = AccountEvent {
            event_id: new_id("evt"),
            event_type: position_event,
            order_id: Some(order_id.clone()),
            fill_id: Some(fill.fill_id.clone()),
            position_id: Some(position_id.clone()),
            ts_code: Some(order.ts_code.clone()),
            reason: Some("limit fill".into()),
            actor: AccountActor::System.as_str().into(),
            payload: json!({
                "side": match order.side { OrderSide::Buy => "buy", OrderSide::Sell => "sell" },
                "price": exec_price.0.to_string(),
                "quantity": exec_quantity.0,
            }),
            occurred_at: now,
        };
        AccountRepository::append_event(tx, &pos_ev)?;
        event_ids.push(pos_ev.event_id);

        // Lots
        match order.side {
            OrderSide::Buy => {
                let ctx = resolve_market_time(now);
                let trade_date = ctx
                    .current_trade_date
                    .unwrap_or(ctx.latest_completed_trade_date);
                let sellable_from = ctx.next_trade_date.unwrap_or(trade_date);
                let lot = PositionLot {
                    lot_id: new_id("lot"),
                    position_id: position_id.clone(),
                    ts_code: order.ts_code.clone(),
                    source_fill_id: fill.fill_id.clone(),
                    trade_date,
                    quantity: exec_quantity,
                    remaining_quantity: exec_quantity,
                    frozen_quantity: Shares(0),
                    sellable_from,
                    created_at: now,
                };
                AccountRepository::insert_lot(tx, &lot)?;
            }
            OrderSide::Sell => {
                consume_lots_fifo(tx, &position_id, exec_quantity)?;
            }
        }

        // Adjust freeze (release as fill happens)
        if let Some(f) = &freeze_before {
            match f.side {
                OrderSide::Buy => {
                    // 释放对应金额：实际成交金额 + 实际佣金；剩余冻结金额（按 limitPrice 原冻结 - 实际占用）。
                    let actual_use = trade_amount + commission.0;
                    let remaining_cash = (f.frozen_cash.0 - actual_use).max(Decimal::ZERO);
                    if full_fill {
                        // 释放剩余冻结
                        if remaining_cash > Decimal::ZERO {
                            let ev = AccountEvent {
                                event_id: new_id("evt"),
                                event_type: AccountEventType::CashReleased,
                                order_id: Some(order_id.clone()),
                                fill_id: Some(fill.fill_id.clone()),
                                position_id: None,
                                ts_code: Some(order.ts_code.clone()),
                                reason: Some("partial freeze release on fill".into()),
                                actor: AccountActor::System.as_str().into(),
                                payload: json!({ "amount": remaining_cash.to_string() }),
                                occurred_at: now,
                            };
                            AccountRepository::append_event(tx, &ev)?;
                            event_ids.push(ev.event_id);
                        }
                        AccountRepository::delete_freeze(tx, &order_id)?;
                    } else {
                        // 部分成交：扣减冻结至剩余 limit * remaining_qty + estimated_fee 比例
                        let new_remaining_qty = updated_order.quantity.0 - updated_order.filled_quantity.0;
                        let new_frozen = order.limit_price.map(|p| p.0).unwrap_or(Decimal::ZERO)
                            * Decimal::from(new_remaining_qty);
                        AccountRepository::upsert_freeze(
                            tx,
                            &FreezeEntry {
                                order_id: order_id.clone(),
                                ts_code: order.ts_code.clone(),
                                side: OrderSide::Buy,
                                frozen_cash: Money(new_frozen),
                                frozen_shares: Shares(0),
                                frozen_lots: vec![],
                            },
                        )?;
                    }
                }
                OrderSide::Sell => {
                    // 释放等量 frozen lots
                    if full_fill {
                        AccountRepository::delete_freeze(tx, &order_id)?;
                    } else {
                        // partially — leave freeze as-is for simplicity; lots已通过 consume_lots_fifo 扣减。
                        // 实务可以再细化 lot-by-lot释放；保留 freeze 记录但更新 frozen_shares。
                        let new_remaining_qty = updated_order.quantity.0 - updated_order.filled_quantity.0;
                        AccountRepository::upsert_freeze(
                            tx,
                            &FreezeEntry {
                                order_id: order_id.clone(),
                                ts_code: order.ts_code.clone(),
                                side: OrderSide::Sell,
                                frozen_cash: Money(Decimal::ZERO),
                                frozen_shares: Shares(new_remaining_qty),
                                frozen_lots: f.frozen_lots.clone(),
                            },
                        )?;
                    }
                }
            }
        }

        // order_filled / order_partially_filled event
        let ev_type = if full_fill {
            AccountEventType::OrderFilled
        } else {
            AccountEventType::OrderPartiallyFilled
        };
        let fill_ev = AccountEvent {
            event_id: order_filled_ev_id.clone(),
            event_type: ev_type,
            order_id: Some(order_id.clone()),
            fill_id: Some(fill.fill_id.clone()),
            position_id: Some(position_id.clone()),
            ts_code: Some(order.ts_code.clone()),
            reason: Some(if full_fill { "limit filled" } else { "limit partial" }.into()),
            actor: AccountActor::System.as_str().into(),
            payload: json!({
                "price": exec_price.0.to_string(),
                "quantity": exec_quantity.0,
            }),
            occurred_at: now,
        };
        AccountRepository::append_event(tx, &fill_ev)?;
        event_ids.push(fill_ev.event_id.clone());

        // trigger only on order_filled terminal (not partially)
        if full_fill {
            let trig = AccountTrigger {
                trigger_id: TriggerKey::OrderTerminal {
                    trigger_type: AccountTriggerType::OrderFilled,
                    order_id: &order_id,
                    ts_code: &order.ts_code,
                    event_id: &fill_ev.event_id,
                }
                .stable_id(),
                trigger_type: AccountTriggerType::OrderFilled,
                order_id: Some(order_id.clone()),
                position_id: Some(position_id.clone()),
                ts_code: Some(order.ts_code.clone()),
                price: Some(exec_price),
                threshold: None,
                quote_freshness: None,
                warnings: vec![],
                event_id: fill_ev.event_id.clone(),
                handled: false,
                occurred_at: now,
            };
            if AccountRepository::insert_trigger_if_new(tx, &trig)? {
                trigger = Some(trig);
            }
        }
        Ok(())
    });
    if result.is_err() {
        return None;
    }
    // Update cash
    let _ = repo.update_cash(Money(meta.cash.0 + cash_delta), now);
    Some((trigger, event_ids))
}

/// Spec: account-module.md §2 仓位模型: lot cost = commission + transfer_fee；
///   realizedPnl = (price - avgCost) * qty - sellCommission - stampTax - sellTransferFee。
#[allow(clippy::too_many_arguments)]
fn derive_position_after_fill(
    side: OrderSide,
    existing: &Option<Position>,
    ts_code: &TsCode,
    price: Price,
    quantity: Shares,
    commission: Money,
    stamp_tax: Money,
    transfer_fee: Money,
    now: OccurredAt,
    position_id: String,
    instrument_name: String,
) -> (AccountEventType, Position) {
    match (side, existing) {
        (OrderSide::Buy, None) => {
            let avg = apply_buy_avg_cost(
                Shares(0),
                Price(Decimal::ZERO),
                quantity,
                price,
                commission,
                transfer_fee,
            );
            (
                AccountEventType::PositionOpened,
                Position {
                    position_id,
                    ts_code: ts_code.clone(),
                    name: instrument_name,
                    status: PositionStatus::Open,
                    quantity,
                    sellable_quantity: Shares(0),
                    avg_cost: avg,
                    market_price: None,
                    market_value: None,
                    quote_freshness: None,
                    realized_pnl: Money(Decimal::ZERO),
                    unrealized_pnl: None,
                    opened_at: now,
                    closed_at: None,
                    protection: None,
                    actor: TradingActor::Agent,
                    reasoning: None,
                    warnings: vec![],
                },
            )
        }
        (OrderSide::Buy, Some(p)) => {
            let new_qty = Shares(p.quantity.0 + quantity.0);
            let avg = apply_buy_avg_cost(
                p.quantity,
                p.avg_cost,
                quantity,
                price,
                commission,
                transfer_fee,
            );
            let mut pos = p.clone();
            pos.quantity = new_qty;
            pos.avg_cost = avg;
            (AccountEventType::PositionScaled, pos)
        }
        (OrderSide::Sell, Some(p)) => {
            let realized_delta = apply_sell_realized_pnl(
                quantity,
                price,
                p.avg_cost,
                commission,
                stamp_tax,
                transfer_fee,
            );
            let new_qty = Shares(p.quantity.0 - quantity.0);
            let mut pos = p.clone();
            pos.quantity = new_qty;
            pos.realized_pnl = Money(p.realized_pnl.0 + realized_delta.0);
            let evt = if new_qty.0 == 0 {
                pos.status = PositionStatus::Closed;
                pos.closed_at = Some(now);
                AccountEventType::PositionClosed
            } else {
                AccountEventType::PositionScaled
            };
            (evt, pos)
        }
        (OrderSide::Sell, None) => {
            // 不可达
            (
                AccountEventType::PositionClosed,
                Position {
                    position_id,
                    ts_code: ts_code.clone(),
                    name: instrument_name,
                    status: PositionStatus::Closed,
                    quantity: Shares(0),
                    sellable_quantity: Shares(0),
                    avg_cost: Price(Decimal::ZERO),
                    market_price: None,
                    market_value: None,
                    quote_freshness: None,
                    realized_pnl: Money(Decimal::ZERO),
                    unrealized_pnl: None,
                    opened_at: now,
                    closed_at: Some(now),
                    protection: None,
                    actor: TradingActor::Agent,
                    reasoning: None,
                    warnings: vec![],
                },
            )
        }
    }
}

// ----------------------------------------------------------------------------
// Protection evaluation
// ----------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn evaluate_protection(
    repo: &AccountRepository<'_>,
    deps: &EvalDeps,
    pos: &Position,
    prot: &PositionProtection,
    now: OccurredAt,
    triggers_out: &mut Vec<AccountTrigger>,
    events_out: &mut Vec<String>,
    warnings_out: &mut Vec<WarningCode>,
) {
    if !prot.enabled {
        return;
    }
    let ctx = resolve_market_time(now);
    let trade_date = ctx
        .current_trade_date
        .unwrap_or(ctx.latest_completed_trade_date);

    // 1) time_stop（不依赖行情）
    if let Some(ts) = prot.time_stop_at {
        if now >= ts {
            create_protection_trigger(
                repo,
                pos,
                prot,
                AccountTriggerType::TimeStop,
                None,
                Some(ts.to_rfc3339()),
                trade_date,
                None,
                vec![],
                now,
                triggers_out,
                events_out,
            );
        }
    }

    // 2) price-based: stop_loss / take_profit (依赖行情)
    if prot.stop_loss.is_some() || prot.take_profit.is_some() {
        match deps.gateway.get_snapshot(&pos.ts_code) {
            Ok(snap) => {
                let Some(price) = snap.quote.price else {
                    warnings_out.push(WarningCode::QuotePriceMissing);
                    return;
                };
                let mut warns: Vec<WarningCode> = vec![];
                let freshness = snap.quote.freshness.clone();
                if matches!(freshness.status, FreshnessStatus::Stale) {
                    warns.push(WarningCode::QuoteStale);
                }
                if let Some(sl) = prot.stop_loss {
                    if price.0 <= sl.0 {
                        create_protection_trigger(
                            repo,
                            pos,
                            prot,
                            AccountTriggerType::StopLoss,
                            Some(price),
                            Some(sl.0.to_string()),
                            trade_date,
                            Some(freshness.clone()),
                            warns.clone(),
                            now,
                            triggers_out,
                            events_out,
                        );
                    }
                }
                if let Some(tp) = prot.take_profit {
                    if price.0 >= tp.0 {
                        create_protection_trigger(
                            repo,
                            pos,
                            prot,
                            AccountTriggerType::TakeProfit,
                            Some(price),
                            Some(tp.0.to_string()),
                            trade_date,
                            Some(freshness),
                            warns,
                            now,
                            triggers_out,
                            events_out,
                        );
                    }
                }
            }
            Err(e) => {
                let w = quote_err_to_warning(e.kind);
                if !warnings_out.contains(&w) {
                    warnings_out.push(w);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn create_protection_trigger(
    repo: &AccountRepository<'_>,
    pos: &Position,
    prot: &PositionProtection,
    trigger_type: AccountTriggerType,
    price: Option<Price>,
    threshold: Option<String>,
    trade_date: crate::domain::shared::TradeDate,
    freshness: Option<crate::domain::shared::Freshness>,
    warnings: Vec<WarningCode>,
    now: OccurredAt,
    triggers_out: &mut Vec<AccountTrigger>,
    events_out: &mut Vec<String>,
) {
    let key = if matches!(trigger_type, AccountTriggerType::TimeStop) {
        TriggerKey::TimeStop {
            position_id: &pos.position_id,
            ts_code: &pos.ts_code,
            protection_revision: prot.revision,
            time_stop_at: prot.time_stop_at.unwrap_or(now),
        }
    } else {
        TriggerKey::PriceProtection {
            trigger_type,
            position_id: &pos.position_id,
            ts_code: &pos.ts_code,
            protection_revision: prot.revision,
            threshold: threshold.clone().unwrap_or_default(),
            trade_date,
        }
    };
    let trigger_id = key.stable_id();
    let trigger_event_id = new_id("evt");
    let mut inserted = false;
    let mut new_trigger: Option<AccountTrigger> = None;
    let result: rusqlite::Result<()> = repo.tx(|tx| {
        if AccountRepository::trigger_exists(tx, &trigger_id)? {
            return Ok(());
        }
        let trig_ev = AccountEvent {
            event_id: trigger_event_id.clone(),
            event_type: AccountEventType::TriggerCreated,
            order_id: None,
            fill_id: None,
            position_id: Some(pos.position_id.clone()),
            ts_code: Some(pos.ts_code.clone()),
            reason: Some(format!("{:?}", trigger_type)),
            actor: AccountActor::System.as_str().into(),
            payload: json!({
                "triggerType": trigger_type.as_str(),
                "triggerId": trigger_id,
                "threshold": threshold,
                "price": price.map(|p| p.0.to_string()),
                "protectionRevision": prot.revision,
            }),
            occurred_at: now,
        };
        AccountRepository::append_event(tx, &trig_ev)?;
        let trig = AccountTrigger {
            trigger_id: trigger_id.clone(),
            trigger_type,
            order_id: None,
            position_id: Some(pos.position_id.clone()),
            ts_code: Some(pos.ts_code.clone()),
            price,
            threshold,
            quote_freshness: freshness,
            warnings,
            event_id: trig_ev.event_id.clone(),
            handled: false,
            occurred_at: now,
        };
        if AccountRepository::insert_trigger_if_new(tx, &trig)? {
            events_out.push(trig_ev.event_id);
            inserted = true;
            new_trigger = Some(trig);
        }
        Ok(())
    });
    if result.is_ok() && inserted {
        if let Some(t) = new_trigger {
            triggers_out.push(t);
        }
    }
}

fn new_id(prefix: &str) -> String {
    format!("{}_{}", prefix, Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::account::policy::AccountFeePolicy;
    use crate::infrastructure::db::run_migrations;
    use crate::pipeline::account::quote_gateway::MockQuoteGateway;

    fn setup() -> (AppDb, EvalDeps) {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| {
            let mut all = Vec::new();
            all.extend(crate::infrastructure::quotes::migrations());
            all.extend(crate::infrastructure::account::migrations());
            run_migrations(c, all).unwrap();
        });
        let deps = EvalDeps {
            db: db.clone(),
            gateway: Arc::new(MockQuoteGateway::new()),
            fee_policy: AccountFeePolicy::default(),
        };
        // 必须初始化 account_meta，否则 commit_limit_fill 会 None。
        AccountRepository::new(&db)
            .insert_meta(Money(Decimal::from(1_000_000)), Utc::now())
            .unwrap();
        (db, deps)
    }

    #[test]
    fn evaluate_no_orders_returns_empty() {
        let (_db, deps) = setup();
        let r = evaluate_account_triggers(EvalInput {
            deps: &deps,
            now: Utc::now(),
            batch_size: 100,
            cursor: None,
        });
        assert!(r.triggers.is_empty());
        assert!(r.account_event_ids.is_empty());
        assert!(!r.has_more);
    }

    #[test]
    fn batch_size_caps_processing() {
        let (_db, deps) = setup();
        let r = evaluate_account_triggers(EvalInput {
            deps: &deps,
            now: Utc::now(),
            batch_size: 0,
            cursor: None,
        });
        assert!(r.triggers.is_empty());
    }

    // ------------------------------------------------------------------
    // Cursor encode / parse (Drift J)
    // ------------------------------------------------------------------

    #[test]
    fn eval_cursor_round_trip() {
        let c = EvalCursor {
            phase: EvalPhase::Orders,
            anchor_ts: "2026-05-27T01:23:45+00:00".into(),
            anchor_key: "ord_abc".into(),
        };
        let s = c.encode();
        assert!(s.starts_with("eval/orders/"));
        let parsed = EvalCursor::parse(&s).unwrap();
        assert_eq!(parsed.phase, EvalPhase::Orders);
        assert_eq!(parsed.anchor_ts, c.anchor_ts);
        assert_eq!(parsed.anchor_key, c.anchor_key);
    }

    #[test]
    fn eval_cursor_phase_positions_encodes() {
        let c = EvalCursor {
            phase: EvalPhase::Positions,
            anchor_ts: "2026-05-27T01:23:45+00:00".into(),
            anchor_key: "pos_xyz".into(),
        };
        let s = c.encode();
        let parsed = EvalCursor::parse(&s).unwrap();
        assert_eq!(parsed.phase, EvalPhase::Positions);
    }

    #[test]
    fn eval_cursor_bad_format_returns_none() {
        assert!(EvalCursor::parse("garbage").is_none());
        assert!(EvalCursor::parse("eval/unknown_phase/ts/key").is_none());
        assert!(EvalCursor::parse("eval/orders/only_two").is_none());
    }

    // ------------------------------------------------------------------
    // T6 — protection trigger idempotency within same revision/day.
    // Spec: §2 触发事件模型 — 同 revision/threshold/tradeDate 最多一次。
    // ------------------------------------------------------------------

    fn make_position(code: &TsCode, position_id: &str) -> Position {
        Position {
            position_id: position_id.into(),
            ts_code: code.clone(),
            name: "x".into(),
            status: PositionStatus::Open,
            quantity: Shares(1000),
            sellable_quantity: Shares(1000),
            avg_cost: Price(Decimal::new(100, 0)),
            market_price: None,
            market_value: None,
            quote_freshness: None,
            realized_pnl: Money(Decimal::ZERO),
            unrealized_pnl: None,
            opened_at: Utc::now(),
            closed_at: None,
            protection: None,
            actor: TradingActor::Agent,
            reasoning: None,
            warnings: vec![],
        }
    }

    fn make_protection(stop_loss: Option<f64>, revision: u32) -> PositionProtection {
        PositionProtection {
            stop_loss: stop_loss.map(|p| Price(Decimal::from_str_exact(&p.to_string()).unwrap())),
            take_profit: None,
            time_stop_at: None,
            invalidation_signals: vec![],
            enabled: true,
            revision,
            updated_at: Utc::now(),
        }
    }

    fn seed_inst_for_eval(db: &AppDb, ts: &str) -> TsCode {
        let code = TsCode::parse(ts).unwrap();
        use crate::domain::quotes::{InstrumentSource as Q_InstrumentSource, MarketInstrument as Q_MarketInstrument};
        use crate::domain::shared::{InstrumentCategory, InstrumentStatus, Market};
        crate::infrastructure::quotes::QuotesRepository::new(db)
            .upsert_instruments(&[Q_MarketInstrument {
                ts_code: code.clone(),
                name: "Test".into(),
                category: InstrumentCategory::Stock,
                market: Market::SH,
                board: None,
                sector: None,
                status: Some(InstrumentStatus::Listed),
                is_st: Some(false),
                publisher: None,
                index_category: None,
                fund_type: None,
                management: None,
                list_date: None,
                source: Q_InstrumentSource::Tushare,
                updated_at: Utc::now(),
            }])
            .unwrap();
        code
    }

    fn snap_with_price_and_freshness(
        code: &TsCode,
        price: f64,
        freshness: FreshnessStatus,
    ) -> crate::domain::quotes::MarketQuoteSnapshot {
        use crate::domain::quotes::{QuoteDepthLevel, QuoteSource, StockQuote, TradeStatus};
        use crate::domain::shared::{Freshness, InstrumentCategory, TradeDate, Volume};
        let now = Utc::now();
        let p = Price(Decimal::from_str_exact(&price.to_string()).unwrap());
        crate::domain::quotes::MarketQuoteSnapshot {
            ts_code: code.clone(),
            category: InstrumentCategory::Stock,
            quote: StockQuote {
                ts_code: code.clone(),
                name: None,
                category: InstrumentCategory::Stock,
                trade_date: TradeDate::parse("20260526").unwrap(),
                price: Some(p),
                previous_close: None,
                open: None,
                high: None,
                low: None,
                change: None,
                change_percent: None,
                volume: None,
                amount: None,
                turnover_rate: None,
                volume_ratio: None,
                limit_up: None,
                limit_down: None,
                bid: vec![QuoteDepthLevel {
                    price: Some(p),
                    volume: Some(Volume(10_000)),
                }],
                ask: vec![QuoteDepthLevel {
                    price: Some(p),
                    volume: Some(Volume(10_000)),
                }],
                trade_status: TradeStatus::Trading,
                source: QuoteSource::Tdx,
                captured_at: now,
                exchange_time: None,
                freshness: Freshness {
                    status: freshness,
                    captured_at: Some(now),
                    exchange_time: None,
                    age_ms: None,
                    source: Some("tdx".into()),
                    warning: None,
                },
                warnings: vec![],
            },
            updated_at: now,
        }
    }

    #[test]
    fn protection_trigger_idempotent_within_same_revision() {
        let (db, deps) = setup();
        let code = seed_inst_for_eval(&db, "600519.SH");
        let repo = AccountRepository::new(&db);
        let pos = make_position(&code, "pos1");
        let prot = make_protection(Some(95.0), 1);
        repo.tx(|tx| {
            AccountRepository::upsert_position(tx, &pos)?;
            AccountRepository::upsert_protection(tx, "pos1", &prot)?;
            Ok(())
        })
        .unwrap();
        // Inject fresh quote that triggers stop_loss (price 90 <= stop_loss 95)
        let gw_mock = crate::pipeline::account::quote_gateway::MockQuoteGateway::new();
        gw_mock.set(&code, Ok(snap_with_price_and_freshness(&code, 90.0, FreshnessStatus::Fresh)));
        let deps2 = EvalDeps {
            db: db.clone(),
            gateway: Arc::new(gw_mock),
            fee_policy: AccountFeePolicy::default(),
        };
        let r1 = evaluate_account_triggers(EvalInput {
            deps: &deps2,
            now: Utc::now(),
            batch_size: 10,
            cursor: None,
        });
        // 1st eval — generates stop_loss trigger
        let stop_count_1: usize = r1
            .triggers
            .iter()
            .filter(|t| matches!(t.trigger_type, AccountTriggerType::StopLoss))
            .count();
        assert_eq!(stop_count_1, 1, "first eval must produce one stop_loss trigger");
        // 2nd eval — same revision + same threshold + same date → NO new trigger
        let r2 = evaluate_account_triggers(EvalInput {
            deps: &deps2,
            now: Utc::now(),
            batch_size: 10,
            cursor: None,
        });
        let stop_count_2: usize = r2
            .triggers
            .iter()
            .filter(|t| matches!(t.trigger_type, AccountTriggerType::StopLoss))
            .count();
        assert_eq!(stop_count_2, 0, "second eval must NOT duplicate stop_loss trigger");
        let _ = deps;
    }

    // ------------------------------------------------------------------
    // T7 — protection trigger on stale quote emits warning.
    // Spec: §5 行情边界 — stale quote 命中价格型保护条件时仍可以生成 trigger；
    //   AccountTrigger.warnings 必须包含 quote_stale。
    // ------------------------------------------------------------------

    #[test]
    fn protection_trigger_on_stale_quote_emits_warning() {
        let (db, deps) = setup();
        let code = seed_inst_for_eval(&db, "600519.SH");
        let repo = AccountRepository::new(&db);
        let pos = make_position(&code, "pos_stale");
        let prot = make_protection(Some(95.0), 1);
        repo.tx(|tx| {
            AccountRepository::upsert_position(tx, &pos)?;
            AccountRepository::upsert_protection(tx, "pos_stale", &prot)?;
            Ok(())
        })
        .unwrap();
        let gw_mock = crate::pipeline::account::quote_gateway::MockQuoteGateway::new();
        gw_mock.set(&code, Ok(snap_with_price_and_freshness(&code, 90.0, FreshnessStatus::Stale)));
        let deps2 = EvalDeps {
            db: db.clone(),
            gateway: Arc::new(gw_mock),
            fee_policy: AccountFeePolicy::default(),
        };
        let r = evaluate_account_triggers(EvalInput {
            deps: &deps2,
            now: Utc::now(),
            batch_size: 10,
            cursor: None,
        });
        let stop = r
            .triggers
            .iter()
            .find(|t| matches!(t.trigger_type, AccountTriggerType::StopLoss))
            .expect("stale quote should still produce stop_loss trigger");
        assert!(
            stop.warnings.contains(&WarningCode::QuoteStale),
            "stale-quote trigger must carry quote_stale warning"
        );
        let _ = deps;
    }

    /// Cursor resume — given two orders, request batch_size = 1 first → cursor returned;
    /// second call with cursor must skip the first and process only the second.
    #[test]
    fn evaluate_triggers_resumes_from_cursor() {
        let (db, deps) = setup();
        let code = crate::domain::shared::TsCode::parse("600519.SH").unwrap();
        // Seed two pending limit orders that will both expire (so they're processed)
        let now = Utc::now();
        let expired = now - chrono::Duration::seconds(60);
        let repo = AccountRepository::new(&db);
        for i in 0..2 {
            let order = crate::domain::account::types::Order {
                order_id: format!("ord_{}", i),
                ts_code: code.clone(),
                side: OrderSide::Buy,
                order_type: OrderType::Limit,
                limit_price: Some(Price(Decimal::new(100, 0))),
                quantity: Shares(100),
                filled_quantity: Shares(0),
                status: OrderStatus::Pending,
                intent: crate::domain::account::types::OrderIntent::DirectOrder,
                position_id: None,
                reason: Some("x".into()),
                actor: TradingActor::Agent,
                created_at: now - chrono::Duration::seconds(120 - i * 10),
                updated_at: now - chrono::Duration::seconds(120 - i * 10),
                expires_at: Some(expired),
            };
            repo.tx(|tx| {
                AccountRepository::upsert_order(tx, &order)?;
                Ok(())
            })
            .unwrap();
        }
        // First call: batch_size = 1 → should return has_more + cursor
        let r1 = evaluate_account_triggers(EvalInput {
            deps: &deps,
            now,
            batch_size: 1,
            cursor: None,
        });
        assert_eq!(r1.triggers.len(), 1, "first batch should yield 1 trigger");
        assert!(r1.has_more);
        let cursor = r1.next_cursor.expect("must have cursor when has_more");
        // Second call: resume → must skip the first, return the second
        let r2 = evaluate_account_triggers(EvalInput {
            deps: &deps,
            now,
            batch_size: 10,
            cursor: Some(cursor),
        });
        assert_eq!(r2.triggers.len(), 1, "resumed batch should yield the remaining order");
        // ensure not duplicated
        assert_ne!(
            r1.triggers[0].order_id, r2.triggers[0].order_id,
            "resume must process a different order"
        );
    }
}
