//! `AccountService`——模拟账户的**唯一写入口**（含 Mutex 写锁保护并发）。
//!
//! 五个核心写操作：
//! - `open_position`     开新仓
//! - `close_position`    全平
//! - `scale_position`    加 / 减仓
//! - `adjust_stops`      调止损 / 止盈 / 时间止损
//! - `reset`             一键清空（关闭所有 open 仓 + 清掉 closed 历史）
//!
//! 一个核心读操作：
//! - `snapshot`          派生 AccountSnapshot（cash + market_value + realized/unrealized PnL）
//!
//! 设计原则：
//! - 所有写操作 acquire 进程级 `ACCOUNT_WRITE_LOCK`——序列化执行避免 race
//! - 写操作流程：**校验规则 → 同事务 append event + 更新 positions → emit 事件**
//!   事务内部事件先于状态，符合 spec § 1 "持久化先 event 后 state"

use crate::domain::account::cash::reduce_events_to_cash_delta;
use crate::domain::account::errors::{AccountError, RuleError};
use crate::domain::account::position::{Direction, PositionKind};
use crate::domain::account::types::{
    AccountSnapshot, CloseReason, EventSource, Position, PositionId,
};
use crate::domain::account::{
    Account, AdjustStopsCommand, ClosePositionCommand, OpenPositionCommand, ScalePositionCommand,
    TradeQuote,
};
use crate::domain::shared::signal::SignalKind;
use crate::domain::quotes::StockQuote;
use crate::domain::shared::{Lots, OccurredAt, Shares, Yuan};
use crate::infrastructure::account::{
    compute_snapshot, snapshot_cache, PositionRepo, INITIAL_CASH,
};
use crate::infrastructure::quotes::snapshot::market_snapshot;
use serde_json::json;
use std::sync::OnceLock;
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;

pub const EVENT_POSITIONS_CHANGED: &str = "positions-changed";
pub const EVENT_ACCOUNT_SNAPSHOT_UPDATED: &str = "account-snapshot-updated";
/// spec `shared-types.md §6` 跨模块账户更新事件名。
pub const EVENT_ACCOUNT_UPDATED: &str = "account-updated";

/// 统一 emit `account-triggered` 事件 —— spec `shared-types.md AccountTriggeredPayload`：
/// `triggerId / positionId / orderId / tsCode / triggerType / quoteFreshness / warnings`。
/// 任何路径（close path / evaluate / 主动评估）都必须走这里；spec 字段集严格，不附加
/// price / threshold / occurredAt（消费方按 trigger_id 反查 AccountTrigger 拿详情）。
pub fn emit_account_triggered(
    app: &AppHandle,
    trig: &crate::domain::account::trigger::AccountTrigger,
) {
    use crate::domain::account::trigger::AccountTriggerType;
    use crate::domain::shared::{AccountTriggerKind, AccountTriggeredPayload};
    let trigger_type = match trig.trigger_type {
        AccountTriggerType::StopLoss => AccountTriggerKind::StopLoss,
        AccountTriggerType::TakeProfit => AccountTriggerKind::TakeProfit,
        AccountTriggerType::TimeStop => AccountTriggerKind::TimeStop,
        AccountTriggerType::OrderFilled => AccountTriggerKind::OrderFilled,
        AccountTriggerType::OrderRejected => AccountTriggerKind::OrderRejected,
        AccountTriggerType::OrderExpired => AccountTriggerKind::OrderExpired,
        AccountTriggerType::Invalidated => AccountTriggerKind::Invalidated,
    };
    let payload = AccountTriggeredPayload {
        trigger_id: trig.trigger_id.clone(),
        position_id: trig.position_id.clone(),
        order_id: trig.order_id.clone(),
        ts_code: trig.ts_code.clone(),
        trigger_type,
        quote_freshness: trig.quote_freshness.clone(),
        warnings: if trig.warnings.is_empty() {
            None
        } else {
            Some(trig.warnings.clone())
        },
    };
    let _ = app.emit("account-triggered", &payload);
}

/// spec `account-module.md §4 AccountTriggerResult` 分页投影 —— scheduler 当前只读
/// `has_more`（emit 由 service 内部统一），`triggers` / `next_cursor` 保留以满足
/// spec 字段集，供 future cursor-based caller 使用。
#[allow(dead_code)]
pub struct AccountTriggerPage {
    pub triggers: Vec<crate::domain::account::trigger::AccountTrigger>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

/// 开仓请求参数。
pub struct OpenRequest {
    pub code: String,
    pub shares: Shares,
    /// 留空则用 quote.name
    pub name: String,
    /// Live = 真持仓；Watch = "看好但不买"
    pub kind: PositionKind,
    pub direction: Direction,
    pub reasoning: String,
    pub signals_used: Vec<SignalKind>,
    pub invalidation_signals: Vec<SignalKind>,
    pub stop_loss: Option<Yuan>,
    pub take_profit: Option<Yuan>,
    /// 留空则自动算 entered_at + 7 日历日
    pub time_stop_at: Option<OccurredAt>,
    pub source: EventSource,
    pub source_analysis_id: String,
    /// agent 写在 opened 事件上的 markdown 备注（复盘信号）
    pub agent_note_md: String,
}

// ============================================================================
// AccountService
// ============================================================================

pub struct AccountService {
    app: AppHandle,
    repo: PositionRepo,
}

impl AccountService {
    pub fn new(app: AppHandle) -> Self {
        let repo = PositionRepo::new(app.clone());
        Self { app, repo }
    }

    // ========================================================================
    // 读：AccountSnapshot 派生
    // ========================================================================

    /// 当前账户快照——派生计算，O(N) walk 事件链。
    pub fn snapshot(&self) -> Result<AccountSnapshot, AccountError> {
        let positions = self.repo.list_all()?;
        let ids: Vec<PositionId> = positions.iter().map(|p| p.id.clone()).collect();
        let events = self.repo.list_events_batch(&ids)?;
        Ok(compute_snapshot(&positions, &events))
    }

    /// 当前现金（轻量版——仅 cash，不算 market_value）。
    pub fn current_cash(&self) -> Result<Yuan, AccountError> {
        let positions = self.repo.list_all()?;
        if positions.is_empty() {
            return Ok(Yuan::from_unchecked(INITIAL_CASH));
        }
        let ids: Vec<PositionId> = positions.iter().map(|p| p.id.clone()).collect();
        let events = self.repo.list_events_batch(&ids)?;
        let delta = reduce_events_to_cash_delta(&events);
        Ok(Yuan::from_unchecked(INITIAL_CASH + delta.value()))
    }

    // ========================================================================
    // 写：开仓
    // ========================================================================

    pub async fn open_position(&self, req: OpenRequest) -> Result<Position, AccountError> {
        let _guard = account_write_lock().lock().await;

        let positions = self.repo.list_all()?;
        let quote = self.fetch_quote(&req.code).await?;
        // spec account-module.md §444：即时成交 fail closed on stale / missing quote
        enforce_quote_fresh(&quote, &req.code)?;
        let entry_price = quote_price_yuan(&quote, &req.code)?;
        let cash = self.current_cash()?;
        let mut account = Account::new(positions);
        let mutation = account.open_position(OpenPositionCommand {
            code: req.code,
            shares: req.shares,
            name: req.name,
            kind: req.kind,
            direction: req.direction,
            reasoning: req.reasoning,
            signals_used: req.signals_used,
            invalidation_signals: req.invalidation_signals,
            stop_loss: req.stop_loss,
            take_profit: req.take_profit,
            time_stop_at: req.time_stop_at,
            source: req.source,
            source_analysis_id: req.source_analysis_id,
            agent_note_md: req.agent_note_md,
            quote: TradeQuote {
                name: quote.name.clone(),
                price: entry_price,
                ask_top_volume: ask_top_volume(&quote),
            },
            available_cash: cash,
        })?;

        self.repo
            .commit_event_and_positions(&mutation.event, &mutation.positions)?;
        self.emit_positions_changed();
        Ok(mutation.position)
    }

    // ========================================================================
    // 写：全平
    // ========================================================================

    pub async fn close_position(
        &self,
        position_id: &PositionId,
        reason: CloseReason,
        source: EventSource,
        agent_note_md: String,
    ) -> Result<Position, AccountError> {
        let _guard = account_write_lock().lock().await;
        let positions = self.repo.list_all()?;
        let target = positions
            .iter()
            .find(|p| p.id == *position_id)
            .cloned()
            .ok_or_else(|| RuleError::PositionNotFound(position_id.as_str().to_string()))?;

        let is_watch = matches!(target.kind, PositionKind::Watch);
        // Watch 拿不到 quote 也能 close（无 PnL 影响）；Live 即时成交必须 fresh quote。
        let (exit_price, bid_top, quote_freshness) = match self
            .fetch_quote(target.code.as_str())
            .await
        {
            Ok(q) => {
                if !is_watch {
                    // spec account-module.md §444 fail closed
                    enforce_quote_fresh(&q, target.code.as_str())?;
                }
                let price =
                    quote_price_yuan(&q, target.code.as_str()).unwrap_or(target.avg_entry_price);
                let bid = bid_top_volume(&q);
                (price, bid, Some(q.freshness.clone()))
            }
            Err(_) if is_watch => (target.avg_entry_price, None, None),
            Err(e) => return Err(e),
        };
        let mut account = Account::new(positions);
        let mutation = account.close_position(ClosePositionCommand {
            position_id: position_id.clone(),
            exit_price,
            bid_top_volume: bid_top,
            reason,
            source,
            agent_note_md,
            unchecked: is_watch, // Watch 跳过 T+1 / 盘口 / 交易时段（domain 层也判过，这里加保险）
        })?;
        let event_id_for_trigger = mutation.event.id.clone();
        let position_id_str = mutation.event.position_id.as_str().to_string();
        let ts_code = target.code.as_str().to_string();
        self.repo
            .commit_event_and_positions(&mutation.event, &mutation.positions)?;

        // Spec account-module.md §2: 保护条件 / 失效信号触发的 close 生成 AccountTrigger
        // 并 emit `account-triggered`，让 Agent Runtime 路由后续复盘 run。
        self.maybe_emit_close_trigger(
            reason,
            &position_id_str,
            &ts_code,
            &event_id_for_trigger,
            exit_price,
            quote_freshness.clone(),
        );

        self.emit_positions_changed();
        Ok(mutation.position)
    }

    fn maybe_emit_close_trigger(
        &self,
        reason: CloseReason,
        position_id: &str,
        ts_code: &str,
        event_id: &str,
        exit_price: Yuan,
        quote_freshness: Option<crate::domain::shared::Freshness>,
    ) {
        use crate::domain::account::trigger::{
            derive_trigger_id_from_close, AccountTrigger, AccountTriggerType, TriggerThreshold,
        };
        use crate::domain::shared::{FreshnessStatus, WarningCode};
        let Some(trigger_type) = AccountTriggerType::from_close_reason(reason) else {
            return;
        };
        let trigger_id =
            derive_trigger_id_from_close(trigger_type, position_id, ts_code, event_id);
        let is_price_type = matches!(
            trigger_type,
            AccountTriggerType::StopLoss | AccountTriggerType::TakeProfit
        );
        let threshold = if is_price_type {
            Some(TriggerThreshold::Price {
                value: exit_price.value(),
            })
        } else if matches!(trigger_type, AccountTriggerType::TimeStop) {
            Some(TriggerThreshold::Time {
                at: chrono::Utc::now().to_rfc3339(),
            })
        } else {
            None
        };
        // spec §10：stale quote 命中价格型保护条件时 trigger 仍 emit，但必须带 quote_stale warning
        let mut warnings: Vec<WarningCode> = Vec::new();
        if is_price_type {
            match &quote_freshness {
                Some(f) if matches!(f.status, FreshnessStatus::Stale) => {
                    warnings.push(WarningCode::QuoteStale);
                }
                Some(f) if matches!(f.status, FreshnessStatus::Missing) => {
                    warnings.push(WarningCode::QuoteMissing);
                }
                _ => {}
            }
        }
        // spec §2「保护条件 / 失效信号 trigger 必须写 trigger_created 事件，并使用该
        // trigger_created.eventId」—— 先 append AccountEvent，再用其 event_id 写 trigger。
        use crate::domain::account::account_event::{AccountEvent, AccountEventType};
        use crate::domain::account::events::AccountActor;
        use crate::infrastructure::account::account_events_repo;
        let created = AccountEvent::new(
            AccountEventType::TriggerCreated,
            AccountActor::System,
            serde_json::json!({
                "triggerType": trigger_type.as_str(),
                "price": exit_price.value(),
                "warnings": warnings,
            }),
        )
        .with_position(position_id)
        .with_ts_code(ts_code);
        let trigger_event_id = match account_events_repo::append(&self.app, &created) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(
                    target = "account",
                    error = %e,
                    trigger_id = %trigger_id,
                    "写 trigger_created AccountEvent 失败，回退到 position event_id"
                );
                event_id.to_string()
            }
        };
        let trig = AccountTrigger {
            trigger_id: trigger_id.clone(),
            trigger_type,
            order_id: None,
            position_id: Some(position_id.to_string()),
            ts_code: Some(ts_code.to_string()),
            price: Some(exit_price.value()),
            threshold,
            // spec §2「价格型触发必须填写 quoteFreshness」
            quote_freshness: if is_price_type { quote_freshness.clone() } else { None },
            warnings: warnings.clone(),
            event_id: trigger_event_id,
            handled: false,
            occurred_at: chrono::Utc::now().to_rfc3339(),
        };
        match crate::infrastructure::account::trigger_repo::upsert_pending(&self.app, &trig) {
            Ok(true) => {
                emit_account_triggered(&self.app, &trig);
            }
            Ok(false) => {
                // 同一 close event 重复触发：spec 要求幂等，安全跳过
            }
            Err(e) => {
                tracing::warn!(
                    target = "account",
                    error = %e,
                    trigger_id = %trigger_id,
                    "写 account_trigger 失败"
                );
            }
        }
    }

    // ========================================================================
    // 写：加 / 减仓
    // ========================================================================

    pub async fn scale_position(
        &self,
        position_id: &PositionId,
        shares_delta: i64,
        agent_note_md: String,
        source: EventSource,
    ) -> Result<Position, AccountError> {
        let _guard = account_write_lock().lock().await;

        let positions = self.repo.list_all()?;
        let target = positions
            .iter()
            .find(|p| p.id == *position_id)
            .cloned()
            .ok_or_else(|| RuleError::PositionNotFound(position_id.as_str().to_string()))?;
        let quote = self.fetch_quote(target.code.as_str()).await?;
        // spec account-module.md §444 fail closed
        enforce_quote_fresh(&quote, target.code.as_str())?;
        let price = quote_price_yuan(&quote, target.code.as_str())?;
        let cash = self.current_cash()?;
        let mut account = Account::new(positions);
        let ask_top = ask_top_volume(&quote);
        let bid_top = bid_top_volume(&quote);
        let mutation = account.scale_position(ScalePositionCommand {
            position_id: position_id.clone(),
            shares_delta,
            price,
            ask_top_volume: ask_top,
            bid_top_volume: bid_top,
            available_cash: cash,
            source,
            agent_note_md,
        })?;
        self.repo
            .commit_event_and_positions(&mutation.event, &mutation.positions)?;

        self.emit_positions_changed();
        Ok(mutation.position)
    }

    // ========================================================================
    // 写：调止损 / 止盈 / 时间止损
    // ========================================================================

    pub async fn adjust_stops(
        &self,
        position_id: &PositionId,
        stop_loss: Option<Yuan>,
        take_profit: Option<Yuan>,
        time_stop_at: Option<OccurredAt>,
        source: EventSource,
        agent_note_md: String,
    ) -> Result<Position, AccountError> {
        let _guard = account_write_lock().lock().await;

        let positions = self.repo.list_all()?;
        let target = positions
            .iter()
            .find(|p| p.id == *position_id)
            .cloned()
            .ok_or_else(|| RuleError::PositionNotFound(position_id.as_str().to_string()))?;

        // 拿到实时价就校验止损止盈关系（盘外可能拿不到价——放行）
        let current_price = self
            .fetch_quote(target.code.as_str())
            .await
            .ok()
            .and_then(|quote| quote.price);

        let mut account = Account::new(positions);
        let mutation = account.adjust_stops(AdjustStopsCommand {
            position_id: position_id.clone(),
            stop_loss,
            take_profit,
            time_stop_at,
            current_price,
            source,
            agent_note_md,
        })?;
        self.repo
            .commit_event_and_positions(&mutation.event, &mutation.positions)?;

        self.emit_positions_changed();
        Ok(mutation.position)
    }

    // ========================================================================
    // AccountTrigger facade（spec account-module.md §4 内部 Rust API）
    // ========================================================================

    /// 带分页的 evaluate —— 调度层用。
    ///
    /// spec `agent-runtime-module.md §5`：行情 refresh 完成后必须主动评估保护
    /// 条件命中。**主动评估只在 offset = 0 的第一页触发**，避免分页 N 次重复
    /// 评估全 universe。后续分页只 list pending。
    ///
    /// spec §8 in-flight lock `account.trigger_eval` 由调用方（pipeline/scheduler）
    /// 在外层 acquire；Account 模块不依赖 Agent Runtime（architecture.md §2 / CLAUDE.md
    /// 「Quotes / Account / News 不 import Agent 代码」硬约束）。
    pub fn evaluate_account_triggers_paged(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<AccountTriggerPage, String> {
        if offset == 0 {
            if let Err(e) = self.evaluate_protection_conditions() {
                tracing::warn!(error = %e, "保护条件主动评估失败");
            }
        }
        let triggers =
            crate::infrastructure::account::trigger_repo::list_filtered(&self.app, Some(false), limit + 1, offset)?;
        let has_more = triggers.len() as i64 > limit;
        let mut triggers = triggers;
        if has_more {
            triggers.truncate(limit as usize);
        }
        let next_cursor = if has_more {
            Some((offset + limit).to_string())
        } else {
            None
        };
        Ok(AccountTriggerPage {
            triggers,
            has_more,
            next_cursor,
        })
    }

    /// 主动扫保护条件命中 —— spec account-module.md §5「保护条件触发」。
    /// 多头：price <= stopLoss → stop_loss；price >= takeProfit → take_profit；
    /// now >= timeStopAt → time_stop。命中则 derive trigger_id（稳定键）后 upsert，
    /// upsert_pending 幂等保护避免重复。
    fn evaluate_protection_conditions(&self) -> Result<(), String> {
        use crate::domain::account::trigger::{
            derive_price_protection_trigger_id, derive_time_stop_trigger_id, AccountTrigger,
            AccountTriggerType, TriggerThreshold,
        };
        use crate::domain::shared::market_time::resolve_market_time;
        use crate::domain::shared::OccurredAt as OA;
        use crate::infrastructure::quotes::snapshot::market_snapshot;

        let snap = self.snapshot().map_err(|e| e.to_string())?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mkt = resolve_market_time(OA::new(now_ms));
        let trade_date = mkt
            .current_trade_date
            .as_ref()
            .map(|d| d.to_compact())
            .unwrap_or_else(|| mkt.latest_completed_trade_date.to_compact());

        for pos in &snap.open_positions {
            let ts_code = pos.code.as_str();
            let position_id = pos.id.as_str();
            // stop_loss / take_profit 依赖当前 quote
            let q_opt = market_snapshot::get(ts_code);
            // protection_revision 当前 PositionProtection 未持版本号；按 spec 简化以 1 起步
            let revision = 1u32;

            if let (Some(q), Some(sl)) = (q_opt.as_ref(), pos.stop_loss) {
                if let Some(price) = q.price {
                    if price.value() <= sl.value() {
                        let trigger_id = derive_price_protection_trigger_id(
                            AccountTriggerType::StopLoss,
                            position_id,
                            ts_code,
                            revision,
                            sl.value(),
                            &trade_date,
                        );
                        let warnings = if matches!(
                            q.freshness.status,
                            crate::domain::shared::FreshnessStatus::Stale
                        ) {
                            vec![crate::domain::shared::WarningCode::QuoteStale]
                        } else {
                            Vec::new()
                        };
                        let trig = AccountTrigger {
                            trigger_id,
                            trigger_type: AccountTriggerType::StopLoss,
                            order_id: None,
                            position_id: Some(position_id.to_string()),
                            ts_code: Some(ts_code.to_string()),
                            price: Some(price.value()),
                            threshold: Some(TriggerThreshold::Price { value: sl.value() }),
                            quote_freshness: Some(q.freshness.clone()),
                            warnings,
                            event_id: format!(
                                "active_eval:{position_id}:{}:{trade_date}",
                                AccountTriggerType::StopLoss.as_str()
                            ),
                            handled: false,
                            occurred_at: chrono::Utc::now().to_rfc3339(),
                        };
                        if let Ok(true) =
                            crate::infrastructure::account::trigger_repo::upsert_pending(&self.app, &trig)
                        {
                            emit_account_triggered(&self.app, &trig);
                        }
                    }
                }
            }
            if let (Some(q), Some(tp)) = (q_opt.as_ref(), pos.take_profit) {
                if let Some(price) = q.price {
                    if price.value() >= tp.value() {
                        let trigger_id = derive_price_protection_trigger_id(
                            AccountTriggerType::TakeProfit,
                            position_id,
                            ts_code,
                            revision,
                            tp.value(),
                            &trade_date,
                        );
                        let warnings = if matches!(
                            q.freshness.status,
                            crate::domain::shared::FreshnessStatus::Stale
                        ) {
                            vec![crate::domain::shared::WarningCode::QuoteStale]
                        } else {
                            Vec::new()
                        };
                        let trig = AccountTrigger {
                            trigger_id,
                            trigger_type: AccountTriggerType::TakeProfit,
                            order_id: None,
                            position_id: Some(position_id.to_string()),
                            ts_code: Some(ts_code.to_string()),
                            price: Some(price.value()),
                            threshold: Some(TriggerThreshold::Price { value: tp.value() }),
                            quote_freshness: Some(q.freshness.clone()),
                            warnings,
                            event_id: format!(
                                "active_eval:{position_id}:{}:{trade_date}",
                                AccountTriggerType::TakeProfit.as_str()
                            ),
                            handled: false,
                            occurred_at: chrono::Utc::now().to_rfc3339(),
                        };
                        if let Ok(true) =
                            crate::infrastructure::account::trigger_repo::upsert_pending(&self.app, &trig)
                        {
                            emit_account_triggered(&self.app, &trig);
                        }
                    }
                }
            }
            // time_stop：spec §5「time_stop 和 invalidated 不依赖行情 freshness」
            if let Some(ts_at) = pos.time_stop_at {
                if now_ms >= ts_at.value() {
                    let time_stop_at_str = chrono::DateTime::from_timestamp_millis(ts_at.value())
                        .map(|d| d.to_rfc3339())
                        .unwrap_or_default();
                    let trigger_id = derive_time_stop_trigger_id(
                        position_id,
                        ts_code,
                        revision,
                        &time_stop_at_str,
                    );
                    let trig = AccountTrigger {
                        trigger_id,
                        trigger_type: AccountTriggerType::TimeStop,
                        order_id: None,
                        position_id: Some(position_id.to_string()),
                        ts_code: Some(ts_code.to_string()),
                        price: None,
                        threshold: Some(TriggerThreshold::Time {
                            at: time_stop_at_str,
                        }),
                        quote_freshness: None,
                        warnings: Vec::new(),
                        event_id: format!("active_eval:{position_id}:time_stop"),
                        handled: false,
                        occurred_at: chrono::Utc::now().to_rfc3339(),
                    };
                    if let Ok(true) =
                        crate::infrastructure::account::trigger_repo::upsert_pending(&self.app, &trig)
                    {
                        emit_account_triggered(&self.app, &trig);
                    }
                }
            }
        }
        Ok(())
    }

    /// `mark_trigger_handled` —— spec `account-module.md §2/§4`，写 handled=1（幂等）。
    /// 同时 append `trigger_handled` AccountEvent 以满足 spec 21 类型事件流契约。
    pub fn mark_trigger_handled(&self, trigger_id: &str) -> Result<bool, String> {
        use crate::domain::account::account_event::{AccountEvent, AccountEventType};
        use crate::domain::account::events::AccountActor;
        use crate::infrastructure::account::account_events_repo;
        let updated =
            crate::infrastructure::account::trigger_repo::mark_handled(&self.app, trigger_id)?;
        if updated {
            let handled = AccountEvent::new(
                AccountEventType::TriggerHandled,
                AccountActor::Agent,
                serde_json::json!({ "triggerId": trigger_id }),
            );
            if let Err(e) = account_events_repo::append(&self.app, &handled) {
                tracing::warn!(
                    target = "account",
                    error = %e,
                    trigger_id = trigger_id,
                    "append trigger_handled AccountEvent 失败"
                );
            }
        }
        Ok(updated)
    }

    // ========================================================================
    // System lifecycle —— spec §4 内部 Rust API
    // ========================================================================

    /// `initialize_account_if_needed(initial_cash)` —— spec `account-module.md §2/§4`。
    /// 幂等：已存在 `account_initialized` 事件且 initialCash 相同则不写新事件；
    /// 不同则返回 `invalid_input`。
    pub fn initialize_account_if_needed(
        &self,
        initial_cash: f64,
    ) -> Result<AccountSnapshot, AccountError> {
        use crate::domain::account::account_event::{AccountEvent, AccountEventType};
        use crate::domain::account::events::AccountActor;
        use crate::infrastructure::account::account_events_repo;
        if account_events_repo::has_account_initialized(&self.app)
            .map_err(AccountError::Io)?
        {
            if let Some(existing) =
                account_events_repo::get_initial_cash(&self.app).map_err(AccountError::Io)?
            {
                if (existing - initial_cash).abs() > 0.01 {
                    // spec §2 fail closed：已初始化但 initialCash 不一致返回 db_error
                    return Err(AccountError::Io(format!(
                        "account already initialized with initialCash={existing}, request {initial_cash} mismatch"
                    )));
                }
            }
            return self.snapshot();
        }
        let event = AccountEvent::new(
            AccountEventType::AccountInitialized,
            AccountActor::System,
            serde_json::json!({ "initialCash": initial_cash }),
        );
        account_events_repo::append(&self.app, &event).map_err(AccountError::Io)?;
        self.snapshot()
    }

    /// `rebuild_account_snapshot()` —— spec `account-module.md §4`。
    /// 当前 snapshot 派生路径已是 events + market_snapshot 现算；这里只 append
    /// 一条 `snapshot_rebuilt` 审计事件，标记一次显式重建。
    #[allow(dead_code)] // spec §4 内部 Rust API；调用方未来由后台调度触发
    pub fn rebuild_account_snapshot(&self) -> Result<AccountSnapshot, AccountError> {
        use crate::domain::account::account_event::{AccountEvent, AccountEventType};
        use crate::domain::account::events::AccountActor;
        use crate::infrastructure::account::account_events_repo;
        let snap = self.snapshot()?;
        let event = AccountEvent::new(
            AccountEventType::SnapshotRebuilt,
            AccountActor::System,
            serde_json::json!({
                "openPositions": snap.open_positions.len(),
            }),
        );
        if let Err(e) = account_events_repo::append(&self.app, &event) {
            tracing::warn!(
                target = "account",
                error = %e,
                "append snapshot_rebuilt AccountEvent 失败"
            );
        }
        Ok(snap)
    }

    // ========================================================================
    // 写：一键重置
    // ========================================================================

    /// 清空账户——删全部 positions（不平仓，直接清空表）。
    /// 现金重置：因为没有 positions 也就没有 events 影响，自动回 INITIAL_CASH。
    /// 已平仓历史也一并删（重练一遍）。
    pub async fn reset(&self) -> Result<usize, AccountError> {
        let _guard = account_write_lock().lock().await;
        let positions = self.repo.list_all()?;
        let count = positions.len();
        self.repo.clear_all()?;
        self.emit_positions_changed();
        Ok(count)
    }

    // ========================================================================
    // 内部 helpers
    // ========================================================================

    /// 拿单股 quote——优先 MARKET_SNAPSHOT，缺则 lazy ensure 一次（走 dispatch 多源 fallback）。
    async fn fetch_quote(&self, code: &str) -> Result<StockQuote, AccountError> {
        let ts_code =
            crate::infrastructure::quotes::repository::resolve_stock_ts_code(&self.app, code)
                .ok_or_else(|| AccountError::Io(format!("stocks 档案找不到 {code}")))?;
        if let Some(q) = market_snapshot::get(&ts_code) {
            return Ok(q);
        }
        let pairs = crate::infrastructure::quotes::realtime::dispatch()
            .fetch(&[ts_code.clone()])
            .await
            .map_err(|e| AccountError::Io(e.to_string()))?;
        if !pairs.is_empty() {
            market_snapshot::put_batch(pairs.clone());
        }
        pairs
            .into_iter()
            .next()
            .map(|(_, q)| q)
            .ok_or_else(|| RuleError::NoCurrentPrice(code.to_string()).into())
    }

    /// 写操作完成后的收尾 —— emit `account-updated` (canonical spec event with
    /// `AccountUpdatedPayload`), 同时保留旧 `positions-changed` / `account-snapshot-updated`
    /// 兼容前端 hook。立即刷 ACCOUNT_SNAPSHOT cache 避免事件到达与 cache 写入之间 race。
    fn emit_positions_changed(&self) {
        let _ = self.app.emit(EVENT_POSITIONS_CHANGED, json!({}));
        match self.snapshot() {
            Ok(snap) => {
                let captured_at = chrono::Utc::now().to_rfc3339();
                snapshot_cache::put(snap);
                let _ = self.app.emit(EVENT_ACCOUNT_SNAPSHOT_UPDATED, json!({}));
                // canonical AccountUpdatedPayload（spec shared-types §6）
                let _ = self.app.emit(
                    EVENT_ACCOUNT_UPDATED,
                    json!({
                        "accountEventIds": [],
                        "snapshotCapturedAt": captured_at,
                    }),
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "refresh snapshot cache after write failed");
            }
        }
    }
}

static ACCOUNT_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn account_write_lock() -> &'static Mutex<()> {
    ACCOUNT_WRITE_LOCK.get_or_init(|| Mutex::new(()))
}

// ============================================================================
// 价格提取 helper
// ============================================================================

/// spec account-module.md §444 / quotes-module.md §187：交易写路径必须 fail closed
/// —— stale / missing quote 不得成交；tradeStatus = halted / closed / unknown
/// 也不得即时成交。Watch position 不调本函数（无 PnL 影响）。
fn enforce_quote_fresh(quote: &StockQuote, code: &str) -> Result<(), RuleError> {
    use crate::domain::quotes::TradeStatus;
    use crate::domain::shared::FreshnessStatus;
    match quote.freshness.status {
        FreshnessStatus::Fresh => {}
        FreshnessStatus::Stale => {
            return Err(RuleError::QuoteStale {
                code: code.to_string(),
                age_ms: quote.freshness.age_ms,
            })
        }
        FreshnessStatus::Missing => {
            return Err(RuleError::QuoteMissing {
                code: code.to_string(),
            })
        }
    }
    match quote.trade_status {
        TradeStatus::Trading => Ok(()),
        TradeStatus::Halted => Err(RuleError::InstrumentSuspended {
            code: code.to_string(),
        }),
        TradeStatus::Closed | TradeStatus::Unknown => Err(RuleError::OutsideTradingSessionQuote {
            code: code.to_string(),
        }),
    }
}

fn quote_price_yuan(quote: &StockQuote, code: &str) -> Result<Yuan, RuleError> {
    quote
        .price
        .filter(|y| y.value().is_finite() && y.value() > 0.0)
        .ok_or_else(|| RuleError::NoCurrentPrice(code.to_string()))
}

/// 卖一档量——给"买"侧（开仓 / 加仓）填单可行性 check 用。
fn ask_top_volume(quote: &StockQuote) -> Option<Lots> {
    quote.ask_levels.first().and_then(|l| l.volume)
}

/// 买一档量——给"卖"侧（平仓 / 减仓）填单可行性 check 用。
fn bid_top_volume(quote: &StockQuote) -> Option<Lots> {
    quote.bid_levels.first().and_then(|l| l.volume)
}
