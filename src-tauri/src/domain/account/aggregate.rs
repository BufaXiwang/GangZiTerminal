//! Account aggregate root.
//!
//! 聚合根只处理账户内不变量：仓位是否存在、是否可开/平/加减、事件如何生成、
//! positions 快照如何更新。I/O、行情获取、事务提交、事件 emit 都留在 pipeline /
//! infrastructure 层。

use super::errors::{AccountError, RuleError};
use super::rules::{
    commission, compute_new_avg_price, ensure_a_share_code, ensure_fillable, ensure_integer_lot,
    ensure_stops_make_sense, ensure_t_plus_one, ensure_trading_hours, stamp_tax,
};
use super::sizing::derive_time_stop_at;
use super::types::{
    CloseReason, EventSource, Position, PositionEvent, PositionEventKind, PositionId,
    PositionStatus, Side,
};
use crate::domain::shared::{Lots, OccurredAt, Shares, StockCode, Yuan};

/// 开仓 / 加仓买入时给 aggregate 的最少行情数据。
///
/// `ask_top_volume`：卖一档量（`StockQuote.ask_levels[0].volume`）。`None` /
/// 0 量 = 封板或盘口缺失，aggregate 拒交易。
#[derive(Debug, Clone)]
pub struct TradeQuote {
    pub name: String,
    pub price: Yuan,
    pub ask_top_volume: Option<Lots>,
}

#[derive(Debug, Clone)]
pub struct OpenPositionCommand {
    pub code: String,
    pub shares: Shares,
    pub name: String,
    pub thesis: String,
    /// 关联的 Thesis aggregate id（v2 新增）。
    /// agent 主动建仓必须设；用户直接命令建仓可 None。
    pub thesis_id: Option<crate::domain::account::thesis::ThesisId>,
    pub stop_loss: Option<Yuan>,
    pub take_profit: Option<Yuan>,
    pub time_stop_at: Option<OccurredAt>,
    pub source: EventSource,
    pub source_analysis_id: String,
    pub agent_note_md: String,
    pub quote: TradeQuote,
    pub available_cash: Yuan,
}

#[derive(Debug, Clone)]
pub struct ClosePositionCommand {
    pub position_id: PositionId,
    pub exit_price: Yuan,
    /// 买一档量（卖出时填单依据）；None / 0 → 拒交易。`unchecked=true` 时忽略。
    pub bid_top_volume: Option<Lots>,
    pub reason: CloseReason,
    pub source: EventSource,
    pub agent_note_md: String,
    pub unchecked: bool,
}

#[derive(Debug, Clone)]
pub struct ScalePositionCommand {
    pub position_id: PositionId,
    pub shares_delta: i64,
    pub price: Yuan,
    /// 加仓（shares_delta > 0）用——卖一档量。
    pub ask_top_volume: Option<Lots>,
    /// 减仓（shares_delta < 0）用——买一档量。
    pub bid_top_volume: Option<Lots>,
    pub available_cash: Yuan,
    pub source: EventSource,
    pub agent_note_md: String,
}

#[derive(Debug, Clone)]
pub struct AdjustStopsCommand {
    pub position_id: PositionId,
    pub stop_loss: Option<Yuan>,
    pub take_profit: Option<Yuan>,
    pub time_stop_at: Option<OccurredAt>,
    pub current_price: Option<Yuan>,
    pub source: EventSource,
    pub agent_note_md: String,
}

#[derive(Debug, Clone)]
pub struct AccountMutation {
    pub position: Position,
    pub event: PositionEvent,
    pub positions: Vec<Position>,
}

pub struct Account {
    positions: Vec<Position>,
}

impl Account {
    pub fn new(positions: Vec<Position>) -> Self {
        Self { positions }
    }

    pub fn open_position(
        &mut self,
        cmd: OpenPositionCommand,
    ) -> Result<AccountMutation, AccountError> {
        ensure_a_share_code(&cmd.code)?;
        ensure_integer_lot(cmd.shares.value())?;
        ensure_trading_hours()?;

        if self
            .positions
            .iter()
            .any(|p| p.status.is_open() && p.code.as_str() == cmd.code)
        {
            return Err(RuleError::DuplicateOpenCode(cmd.code).into());
        }

        ensure_fillable(cmd.quote.ask_top_volume)?;
        ensure_stops_make_sense(cmd.quote.price, cmd.stop_loss, cmd.take_profit)?;

        let comm = commission(cmd.quote.price, cmd.shares);
        let cost = cmd.quote.price.value() * cmd.shares.value() as f64 + comm.value();
        if cost > cmd.available_cash.value() + f64::EPSILON {
            return Err(RuleError::InsufficientFunds {
                needed: cost,
                available: cmd.available_cash.value(),
            }
            .into());
        }

        let position_id = PositionId::new();
        let entered_at = OccurredAt::now();
        let position = Position {
            id: position_id.clone(),
            code: StockCode::new(&cmd.code).map_err(|e| AccountError::Io(e.to_string()))?,
            name: if cmd.name.is_empty() {
                cmd.quote.name
            } else {
                cmd.name
            },
            avg_entry_price: cmd.quote.price,
            current_shares: cmd.shares,
            status: PositionStatus::Open,
            stop_loss: cmd.stop_loss,
            take_profit: cmd.take_profit,
            time_stop_at: cmd.time_stop_at.or(Some(derive_time_stop_at(entered_at))),
            thesis: cmd.thesis,
            thesis_id: cmd.thesis_id,
            source_analysis_id: cmd.source_analysis_id,
            entered_at,
            last_acquisition_at: entered_at,
        };

        let event = PositionEvent {
            id: uuid::Uuid::new_v4().to_string(),
            position_id,
            kind: PositionEventKind::Opened {
                entry_price: cmd.quote.price,
                shares: cmd.shares,
                commission: comm,
            },
            occurred_at: entered_at,
            source: cmd.source,
            agent_note_md: cmd.agent_note_md,
        };

        self.positions.push(position.clone());
        Ok(AccountMutation {
            position,
            event,
            positions: self.positions.clone(),
        })
    }

    pub fn close_position(
        &mut self,
        cmd: ClosePositionCommand,
    ) -> Result<AccountMutation, AccountError> {
        if !cmd.unchecked {
            ensure_trading_hours()?;
        }

        let target = self.open_position_by_id(&cmd.position_id)?.clone();
        if !cmd.unchecked {
            ensure_t_plus_one(target.last_acquisition_at)?;
            ensure_fillable(cmd.bid_top_volume)?;
        }

        let position_id = target.id.clone();
        let shares = target.current_shares;
        let exit_at = OccurredAt::now();
        let comm = commission(cmd.exit_price, shares);
        let tax = stamp_tax(cmd.exit_price, shares);

        let mut updated = target;
        updated.status = PositionStatus::Closed {
            exit_price: cmd.exit_price,
            exit_at,
            reason: cmd.reason,
        };

        let event = PositionEvent {
            id: uuid::Uuid::new_v4().to_string(),
            position_id: position_id.clone(),
            kind: PositionEventKind::Closed {
                exit_price: cmd.exit_price,
                shares,
                reason: cmd.reason,
                commission: comm,
                stamp_tax: tax,
            },
            occurred_at: exit_at,
            source: cmd.source,
            agent_note_md: cmd.agent_note_md,
        };

        self.replace_position(&position_id, updated.clone());
        Ok(AccountMutation {
            position: updated,
            event,
            positions: self.positions.clone(),
        })
    }

    pub fn scale_position(
        &mut self,
        cmd: ScalePositionCommand,
    ) -> Result<AccountMutation, AccountError> {
        if cmd.shares_delta == 0 {
            return Err(AccountError::Io("shares_delta 不能为 0".into()));
        }
        ensure_trading_hours()?;

        let target = self.open_position_by_id(&cmd.position_id)?.clone();
        let current = target.current_shares.value();
        let new_shares_value = current + cmd.shares_delta;
        if new_shares_value < 0 {
            return Err(RuleError::InsufficientShares {
                holding: current,
                requested: -cmd.shares_delta,
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

        let abs_delta = Shares::from_unchecked(cmd.shares_delta.abs());
        let mut new_position = target.clone();
        new_position.current_shares = Shares::from_unchecked(new_shares_value);

        let event_kind = if cmd.shares_delta > 0 {
            ensure_integer_lot(cmd.shares_delta)?;
            ensure_fillable(cmd.ask_top_volume)?;

            let comm = commission(cmd.price, abs_delta);
            let cost = cmd.price.value() * cmd.shares_delta as f64 + comm.value();
            if cost > cmd.available_cash.value() + f64::EPSILON {
                return Err(RuleError::InsufficientFunds {
                    needed: cost,
                    available: cmd.available_cash.value(),
                }
                .into());
            }

            let new_avg = compute_new_avg_price(
                target.avg_entry_price,
                target.current_shares,
                cmd.price,
                abs_delta,
            );
            new_position.avg_entry_price = new_avg;
            new_position.last_acquisition_at = OccurredAt::now();

            PositionEventKind::ScaledIn {
                delta: abs_delta,
                price: cmd.price,
                new_avg,
                commission: comm,
            }
        } else {
            ensure_t_plus_one(target.last_acquisition_at)?;
            ensure_integer_lot(-cmd.shares_delta)?;
            ensure_fillable(cmd.bid_top_volume)?;

            let comm = commission(cmd.price, abs_delta);
            let tax = stamp_tax(cmd.price, abs_delta);

            PositionEventKind::ScaledOut {
                delta: abs_delta,
                price: cmd.price,
                commission: comm,
                stamp_tax: tax,
            }
        };

        let event = PositionEvent {
            id: uuid::Uuid::new_v4().to_string(),
            position_id: cmd.position_id.clone(),
            kind: event_kind,
            occurred_at: OccurredAt::now(),
            source: cmd.source,
            agent_note_md: cmd.agent_note_md,
        };

        self.replace_position(&cmd.position_id, new_position.clone());
        Ok(AccountMutation {
            position: new_position,
            event,
            positions: self.positions.clone(),
        })
    }

    pub fn adjust_stops(
        &mut self,
        cmd: AdjustStopsCommand,
    ) -> Result<AccountMutation, AccountError> {
        let target = self.open_position_by_id(&cmd.position_id)?.clone();

        if let Some(price) = cmd.current_price {
            ensure_stops_make_sense(
                price,
                cmd.stop_loss.or(target.stop_loss),
                cmd.take_profit.or(target.take_profit),
            )?;
        }

        let mut new_position = target;
        if let Some(sl) = cmd.stop_loss {
            new_position.stop_loss = Some(sl);
        }
        if let Some(tp) = cmd.take_profit {
            new_position.take_profit = Some(tp);
        }
        if let Some(ts) = cmd.time_stop_at {
            new_position.time_stop_at = Some(ts);
        }

        let event = PositionEvent {
            id: uuid::Uuid::new_v4().to_string(),
            position_id: cmd.position_id.clone(),
            kind: PositionEventKind::StopsAdjusted {
                stop_loss: new_position.stop_loss,
                take_profit: new_position.take_profit,
                time_stop_at: new_position.time_stop_at,
            },
            occurred_at: OccurredAt::now(),
            source: cmd.source,
            agent_note_md: cmd.agent_note_md,
        };

        self.replace_position(&cmd.position_id, new_position.clone());
        Ok(AccountMutation {
            position: new_position,
            event,
            positions: self.positions.clone(),
        })
    }

    fn open_position_by_id(&self, id: &PositionId) -> Result<&Position, AccountError> {
        let target = self
            .positions
            .iter()
            .find(|p| p.id == *id)
            .ok_or_else(|| RuleError::PositionNotFound(id.as_str().to_string()))?;
        if !target.status.is_open() {
            return Err(RuleError::PositionAlreadyClosed(id.as_str().to_string()).into());
        }
        Ok(target)
    }

    fn replace_position(&mut self, id: &PositionId, updated: Position) {
        self.positions = self
            .positions
            .iter()
            .cloned()
            .map(|p| if p.id == *id { updated.clone() } else { p })
            .collect();
    }
}
