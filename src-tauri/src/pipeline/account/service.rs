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
use crate::domain::account::types::{
    AccountSnapshot, CloseReason, EventSource, Position, PositionId,
};
use crate::domain::account::{
    Account, AdjustStopsCommand, ClosePositionCommand, OpenPositionCommand, ScalePositionCommand,
    TradeQuote,
};
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

        let positions = self.repo.list_all()?;
        let quote = self.fetch_quote(&req.code).await?;
        let entry_price = quote_price_yuan(&quote, &req.code)?;
        let cash = self.current_cash()?;
        let mut account = Account::new(positions);
        let mutation = account.open_position(OpenPositionCommand {
            code: req.code,
            shares: req.shares,
            name: req.name,
            thesis: req.thesis,
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
        let quote = self.fetch_quote(target.code.as_str()).await?;
        let exit_price = quote_price_yuan(&quote, target.code.as_str())?;
        let mut account = Account::new(positions);
        let bid_top = bid_top_volume(&quote);
        let mutation = account.close_position(ClosePositionCommand {
            position_id: position_id.clone(),
            exit_price,
            bid_top_volume: bid_top,
            reason,
            source,
            agent_note_md,
            unchecked: false,
        })?;
        self.repo
            .commit_event_and_positions(&mutation.event, &mutation.positions)?;

        self.emit_positions_changed();
        Ok(mutation.position)
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
        let mut account = Account::new(positions);
        let mutation = account.close_position(ClosePositionCommand {
            position_id: position_id.clone(),
            exit_price,
            bid_top_volume: None,
            reason,
            source,
            agent_note_md,
            unchecked: true,
        })?;
        self.repo
            .commit_event_and_positions(&mutation.event, &mutation.positions)?;

        self.emit_positions_changed();
        Ok(mutation.position)
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

/// 卖一档量——给"买"侧（开仓 / 加仓）填单可行性 check 用。
fn ask_top_volume(quote: &StockQuote) -> Option<Lots> {
    quote.ask_levels.first().and_then(|l| l.volume)
}

/// 买一档量——给"卖"侧（平仓 / 减仓）填单可行性 check 用。
fn bid_top_volume(quote: &StockQuote) -> Option<Lots> {
    quote.bid_levels.first().and_then(|l| l.volume)
}
