//! Domain `account`——模拟账户 Bounded Context。
//!
//! 定位：**模拟交易终端的训练场**。提供类真实账户的开仓 / 平仓 / 加减仓 / 调止损能力，
//! 由 Agent 自驱动；用户只读 + 管自选股 + 一键重置。
//!
//! 设计原则（per architecture.md § 1）：
//! - **持久化先 event 后 state**：所有写动作 append `PositionEvent`，状态从事件链派生
//! - **派生 over 存储**：cash / realized_pnl / unrealized_pnl 全从事件 + MARKET_SNAPSHOT 算
//! - **模块边界单向**：Account → Quotes（估值读 MARKET_SNAPSHOT）；Quotes 不知 Account
//! - **规则纯函数**：A 股规则（T+1 / 整百 / 涨跌停 / 资金 / 止损止盈合理性）全是纯函数
//!
//! 5 个核心写操作：open / scale_in / scale_out / close / adjust_stops
//! 任一操作触发：
//!   1. 校验规则
//!   2. append PositionEvent
//!   3. 更新 positions 状态
//!   4. emit positions-changed（让 snapshot 重算）
//!
//! 此 mod 只放**纯 domain 类型 + 纯函数**——无 I/O、无 Tauri、无外部副作用。
//! I/O 实现在 `infrastructure::account`，用例编排在 `pipeline::account`。

pub mod cash;
pub mod errors;
pub mod rules;
pub mod sizing;
pub mod types;

pub use errors::{AccountError, RuleError};
pub use types::{
    AccountSnapshot, CloseReason, EventSource, Position, PositionEvent, PositionEventKind,
    PositionId, PositionStatus, Side,
};
