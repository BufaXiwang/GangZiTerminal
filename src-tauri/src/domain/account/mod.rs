//! Account BC domain — 纯类型 + 不变量 + 规则。
//!
//! Spec: docs/design/account-module.md §2 (领域模型) / §5 (模拟成交规则)
//!
//! 依赖方向：domain 层不依赖 tauri / rusqlite / reqwest / infrastructure / pipeline / adapters；
//! 只依赖 `crate::domain::shared` 共享类型。

pub mod errors;
pub mod events;
pub mod money;
pub mod policy;
pub mod requests;
pub mod rules;
pub mod triggers;
pub mod types;

pub use errors::{AccountError, AccountErrorKind};
pub use events::{AccountEvent, AccountEventType};
pub use money::{
    apply_buy_avg_cost, apply_sell_realized_pnl, compute_commission, compute_stamp_tax,
    MoneyMath,
};
pub use policy::{AccountFeePolicy, AccountRiskPolicy, FEE_DEFAULT, RISK_DEFAULT};
pub use requests::{
    AccountActor, FetchAccountInclude, FetchAccountRequest, FetchAccountResponse,
    MarkTriggerHandledRequest, MarkTriggerHandledResponse, OperateAccountAction,
    OperateAccountRequest, OperateAccountResponse, OrderActiveFilter, PositionStatusFilter,
    TriggerHandledFilter, UpdateWatchlistAction, UpdateWatchlistRequest,
    UpdateWatchlistResponse,
};
pub use rules::{
    assert_lot_size, validate_limit_price, validate_quantity_positive, LOT_SIZE,
};
pub use triggers::{AccountTrigger, AccountTriggerResult, AccountTriggerType, TriggerKey};
pub use types::{
    AccountSnapshot, Order, OrderIntent, OrderSide, OrderStatus, OrderType, Position,
    PositionLot, PositionProtection, PositionStatus, TradeFill, TradingActor, WatchlistItem,
    WatchlistItemView, WatchlistQuoteView,
};
