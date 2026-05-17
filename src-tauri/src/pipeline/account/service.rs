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
use crate::domain::account::rules::{
    commission, compute_new_avg_price, ensure_a_share_code, ensure_integer_lot,
    ensure_price_not_limit, ensure_stops_make_sense, ensure_t_plus_one, ensure_trading_hours,
    stamp_tax,
};
use crate::domain::account::sizing::derive_time_stop_at;
use crate::domain::account::types::{
    AccountSnapshot, CloseReason, EventSource, Position, PositionEvent, PositionEventKind,
    PositionId, PositionStatus, Side,
};
use crate::domain::quotes::StockQuote;
use crate::domain::shared::{OccurredAt, Shares, StockCode, Yuan};
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

/// 开仓请求参数。
pub struct OpenRequest {
    pub code: String,
    pub shares: Shares,
    /// 留空则用 quote.name
    pub name: String,
    pub thesis: String,
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

        ensure_a_share_code(&req.code)?;
        ensure_integer_lot(req.shares.value())?;
        ensure_trading_hours()?;

        let positions = self.repo.list_all()?;
        if positions
            .iter()
            .any(|p| p.status.is_open() && p.code.as_str() == req.code)
        {
            return Err(RuleError::DuplicateOpenCode(req.code).into());
        }

        let quote = self.fetch_quote(&req.code).await?;
        let entry_price = quote_price_yuan(&quote, &req.code)?;
        ensure_price_not_limit(quote.change_percent, &req.code, Side::Buy)?;
        ensure_stops_make_sense(entry_price, req.stop_loss, req.take_profit)?;

        let comm = commission(entry_price, req.shares);
        let cost = entry_price.value() * req.shares.value() as f64 + comm.value();
        let cash = self.current_cash()?;
        if cost > cash.value() + f64::EPSILON {
            return Err(RuleError::InsufficientFunds {
                needed: cost,
                available: cash.value(),
            }
            .into());
        }

        let position_id = PositionId::new();
        let entered_at = OccurredAt::now();
        let position = Position {
            id: position_id.clone(),
            code: StockCode::new(&req.code).map_err(|e| AccountError::Io(e.to_string()))?,
            name: if req.name.is_empty() {
                quote.name.clone()
            } else {
                req.name
            },
            avg_entry_price: entry_price,
            current_shares: req.shares,
            status: PositionStatus::Open,
            stop_loss: req.stop_loss,
            take_profit: req.take_profit,
            time_stop_at: req.time_stop_at.or(Some(derive_time_stop_at(entered_at))),
            thesis: req.thesis,
            source_analysis_id: req.source_analysis_id,
            entered_at,
        };

        // Event + state 同一事务提交；事务内部先 insert event，再 replace positions。
        let event = PositionEvent {
            id: uuid::Uuid::new_v4().to_string(),
            position_id: position_id.clone(),
            kind: PositionEventKind::Opened {
                entry_price,
                shares: req.shares,
                commission: comm,
            },
            occurred_at: entered_at,
            source: req.source,
            agent_note_md: req.agent_note_md,
        };
        let mut all = positions;
        all.push(position.clone());
        self.repo.commit_event_and_positions(&event, &all)?;

        self.emit_positions_changed();
        Ok(position)
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
        ensure_trading_hours()?;

        let positions = self.repo.list_all()?;
        let target = positions
            .iter()
            .find(|p| p.id == *position_id)
            .cloned()
            .ok_or_else(|| RuleError::PositionNotFound(position_id.as_str().to_string()))?;
        if !target.status.is_open() {
            return Err(RuleError::PositionAlreadyClosed(position_id.as_str().to_string()).into());
        }
        ensure_t_plus_one(target.entered_at)?;

        let quote = self.fetch_quote(target.code.as_str()).await?;
        let exit_price = quote_price_yuan(&quote, target.code.as_str())?;
        ensure_price_not_limit(quote.change_percent, target.code.as_str(), Side::Sell)?;

        let updated = self
            .apply_close(target, exit_price, reason, source, agent_note_md, positions)
            .await?;
        Ok(updated)
    }

    /// 不校验交易时段 / T+1 / 涨跌停的"系统强平"路径——用于 reset / 未来 risk_scan。
    /// caller 自己保证语义对（reset 接受任意状态；risk_scan 看价格）。
    pub async fn close_position_unchecked(
        &self,
        position_id: &PositionId,
        exit_price: Yuan,
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
        if !target.status.is_open() {
            return Err(RuleError::PositionAlreadyClosed(position_id.as_str().to_string()).into());
        }

        self.apply_close(target, exit_price, reason, source, agent_note_md, positions)
            .await
    }

    async fn apply_close(
        &self,
        target: Position,
        exit_price: Yuan,
        reason: CloseReason,
        source: EventSource,
        agent_note_md: String,
        positions: Vec<Position>,
    ) -> Result<Position, AccountError> {
        let position_id = target.id.clone();
        let shares = target.current_shares;
        let exit_at = OccurredAt::now();
        let comm = commission(exit_price, shares);
        let tax = stamp_tax(exit_price, shares);

        let mut updated = target;
        updated.status = PositionStatus::Closed {
            exit_price,
            exit_at,
            reason,
        };

        let event = PositionEvent {
            id: uuid::Uuid::new_v4().to_string(),
            position_id: position_id.clone(),
            kind: PositionEventKind::Closed {
                exit_price,
                shares,
                reason,
                commission: comm,
                stamp_tax: tax,
            },
            occurred_at: exit_at,
            source,
            agent_note_md,
        };
        let new_positions: Vec<Position> = positions
            .into_iter()
            .map(|p| {
                if p.id == position_id {
                    updated.clone()
                } else {
                    p
                }
            })
            .collect();
        self.repo
            .commit_event_and_positions(&event, &new_positions)?;

        self.emit_positions_changed();
        Ok(updated)
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
        if shares_delta == 0 {
            return Err(AccountError::Io("shares_delta 不能为 0".into()));
        }
        let _guard = account_write_lock().lock().await;
        ensure_trading_hours()?;

        let positions = self.repo.list_all()?;
        let target = positions
            .iter()
            .find(|p| p.id == *position_id)
            .cloned()
            .ok_or_else(|| RuleError::PositionNotFound(position_id.as_str().to_string()))?;
        if !target.status.is_open() {
            return Err(RuleError::PositionAlreadyClosed(position_id.as_str().to_string()).into());
        }

        let current = target.current_shares.value();
        let new_shares_value = current + shares_delta;
        if new_shares_value < 0 {
            return Err(RuleError::InsufficientShares {
                holding: current,
                requested: -shares_delta,
            }
            .into());
        }
        if new_shares_value == 0 {
            return Err(RuleError::ScaleWouldZero.into());
        }
        if new_shares_value % 100 != 0 {
            return Err(RuleError::SharesNotIntegerLot {
                shares: new_shares_value,
            }
            .into());
        }

        let quote = self.fetch_quote(target.code.as_str()).await?;
        let price = quote_price_yuan(&quote, target.code.as_str())?;

        let abs_delta = Shares::from_unchecked(shares_delta.abs());
        let mut new_position = target.clone();
        new_position.current_shares = Shares::from_unchecked(new_shares_value);

        let event_kind = if shares_delta > 0 {
            // 加仓
            ensure_integer_lot(shares_delta)?;
            ensure_price_not_limit(quote.change_percent, target.code.as_str(), Side::Buy)?;

            let comm = commission(price, abs_delta);
            let cost = price.value() * shares_delta as f64 + comm.value();
            let cash = self.current_cash()?;
            if cost > cash.value() + f64::EPSILON {
                return Err(RuleError::InsufficientFunds {
                    needed: cost,
                    available: cash.value(),
                }
                .into());
            }

            let new_avg = compute_new_avg_price(
                target.avg_entry_price,
                target.current_shares,
                price,
                abs_delta,
            );
            new_position.avg_entry_price = new_avg;

            PositionEventKind::ScaledIn {
                delta: abs_delta,
                price,
                new_avg,
                commission: comm,
            }
        } else {
            // 减仓
            ensure_t_plus_one(target.entered_at)?;
            ensure_integer_lot(-shares_delta)?;
            ensure_price_not_limit(quote.change_percent, target.code.as_str(), Side::Sell)?;

            let comm = commission(price, abs_delta);
            let tax = stamp_tax(price, abs_delta);

            PositionEventKind::ScaledOut {
                delta: abs_delta,
                price,
                commission: comm,
                stamp_tax: tax,
            }
        };

        let event = PositionEvent {
            id: uuid::Uuid::new_v4().to_string(),
            position_id: position_id.clone(),
            kind: event_kind,
            occurred_at: OccurredAt::now(),
            source,
            agent_note_md,
        };
        let new_positions: Vec<Position> = positions
            .into_iter()
            .map(|p| {
                if p.id == *position_id {
                    new_position.clone()
                } else {
                    p
                }
            })
            .collect();
        self.repo
            .commit_event_and_positions(&event, &new_positions)?;

        self.emit_positions_changed();
        Ok(new_position)
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
        if !target.status.is_open() {
            return Err(RuleError::PositionAlreadyClosed(position_id.as_str().to_string()).into());
        }

        // 拿到实时价就校验止损止盈关系（盘外可能拿不到价——放行）
        if let Ok(quote) = self.fetch_quote(target.code.as_str()).await {
            if let Some(price) = quote.price {
                ensure_stops_make_sense(
                    price,
                    stop_loss.or(target.stop_loss),
                    take_profit.or(target.take_profit),
                )?;
            }
        }

        let mut new_position = target.clone();
        if let Some(sl) = stop_loss {
            new_position.stop_loss = Some(sl);
        }
        if let Some(tp) = take_profit {
            new_position.take_profit = Some(tp);
        }
        if let Some(ts) = time_stop_at {
            new_position.time_stop_at = Some(ts);
        }

        let event = PositionEvent {
            id: uuid::Uuid::new_v4().to_string(),
            position_id: position_id.clone(),
            kind: PositionEventKind::StopsAdjusted {
                stop_loss: new_position.stop_loss,
                take_profit: new_position.take_profit,
                time_stop_at: new_position.time_stop_at,
            },
            occurred_at: OccurredAt::now(),
            source,
            agent_note_md,
        };
        let new_positions: Vec<Position> = positions
            .into_iter()
            .map(|p| {
                if p.id == *position_id {
                    new_position.clone()
                } else {
                    p
                }
            })
            .collect();
        self.repo
            .commit_event_and_positions(&event, &new_positions)?;

        self.emit_positions_changed();
        Ok(new_position)
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
        let ts_code = crate::infrastructure::quotes::repository::resolve_stock_ts_code(&self.app, code)
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

    /// 写操作完成后的收尾——emit positions-changed + 立即刷 ACCOUNT_SNAPSHOT cache。
    /// 5 个写方法（open/close/scale/adjust/reset）都在 return 前调一次。
    fn emit_positions_changed(&self) {
        let _ = self.app.emit(EVENT_POSITIONS_CHANGED, json!({}));
        // 同步刷新 cache——避免前端在事件到达和 cache 实际写入之间 race
        match self.snapshot() {
            Ok(snap) => {
                snapshot_cache::put(snap);
                let _ = self.app.emit(EVENT_ACCOUNT_SNAPSHOT_UPDATED, json!({}));
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

fn quote_price_yuan(quote: &StockQuote, code: &str) -> Result<Yuan, RuleError> {
    quote
        .price
        .filter(|y| y.value().is_finite() && y.value() > 0.0)
        .ok_or_else(|| RuleError::NoCurrentPrice(code.to_string()))
}
