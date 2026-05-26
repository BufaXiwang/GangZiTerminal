//! Account 模块错误类型。
//!
//! 分两层：
//! - `RuleError`：A 股规则违规——Display 文案给到 LLM / UI，agent 看到能调整决策
//! - `AccountError`：业务错误统一——`RuleError` + IO 错误（DB / 行情 / 序列化）

use std::fmt;

/// 违规——agent 看到这个就知道哪条 A 股规则不让通过，Display 文案给到 LLM。
#[derive(Debug, Clone)]
pub enum RuleError {
    InsufficientFunds {
        needed: f64,
        available: f64,
    },
    TPlusOneViolation {
        entry_date: String,
        today: String,
    },
    OutsideTradingHours,
    /// 对手方盘口为空（封板 / 停牌 / 数据降级），订单填不掉。
    NotFillable,
    SharesNotIntegerLot {
        shares: i64,
    },
    InsufficientShares {
        holding: i64,
        requested: i64,
    },
    PositionNotFound(String),
    PositionAlreadyClosed(String),
    DuplicateOpenCode(String),
    InvalidStops(String),
    NoCurrentPrice(String),
    InvalidCode(String),
    /// scale_position 会让仓位归零——caller 应改用 close_position
    ScaleWouldZero,
    /// spec account-module.md §444：行情 `stale` —— 即时成交 fail closed
    QuoteStale {
        code: String,
        age_ms: Option<i64>,
    },
    /// spec account-module.md §444：行情 `missing` —— 即时成交 fail closed
    QuoteMissing {
        code: String,
    },
    /// spec quotes-module.md §187：`tradeStatus = halted` —— 即时成交 fail closed
    InstrumentSuspended {
        code: String,
    },
    /// spec quotes-module.md §187：`tradeStatus = closed/unknown` 在非交易时段 —— 即时成交 fail closed
    OutsideTradingSessionQuote {
        code: String,
    },
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuleError::InsufficientFunds { needed, available } => write!(
                f,
                "现金 {available:.2} 元不够开 {needed:.2} 元仓，差 {:.2} 元；减少股数或先平别的仓",
                needed - available
            ),
            RuleError::TPlusOneViolation { entry_date, today } => write!(
                f,
                "T+1 规则：开仓日期 {entry_date} 不能在今日 {today} 平仓；明日交易时段再调，或挂条件单"
            ),
            RuleError::OutsideTradingHours => write!(
                f,
                "现在不在 A 股盘中（9:30-11:30 / 13:00-15:00 北京时间）；下个交易时段再发起；盘外只能用 adjust_stops 修改止损止盈，开/平/加减仓都拒"
            ),
            RuleError::NotFillable => write!(
                f,
                "对手方盘口为空——封板 / 停牌 / 数据降级，订单无法成交；换标的或等数据恢复"
            ),
            RuleError::SharesNotIntegerLot { shares } => write!(
                f,
                "股数 {shares} 不合 A 股整手规则：必须 ≥ 100 且为 100 的整数倍（最后一笔减仓除外）"
            ),
            RuleError::InsufficientShares { holding, requested } => write!(
                f,
                "当前持仓 {holding} 股，无法减仓 {requested} 股"
            ),
            RuleError::PositionNotFound(id) => write!(f, "未找到持仓 {id}"),
            RuleError::PositionAlreadyClosed(id) => write!(f, "持仓 {id} 已平仓，无法继续操作"),
            RuleError::DuplicateOpenCode(code) => write!(
                f,
                "已存在 {code} 的 open 仓位；如需追加请用 scale_position 加仓而非 open_position"
            ),
            RuleError::InvalidStops(msg) => write!(f, "止损/止盈不合理：{msg}"),
            RuleError::NoCurrentPrice(code) => write!(f, "拿不到 {code} 的实时价，无法定价"),
            RuleError::InvalidCode(code) => write!(f, "非法 A 股代码：{code}（应为 6 位数字）"),
            RuleError::ScaleWouldZero => write!(
                f,
                "减仓股数会让仓位归零——请改调 close_position 走标准平仓事件链"
            ),
            RuleError::QuoteStale { code, age_ms } => write!(
                f,
                "{code} 行情 stale（age={}ms）；即时成交 fail closed，等下一轮 fresh 行情",
                age_ms.unwrap_or(-1)
            ),
            RuleError::QuoteMissing { code } => write!(
                f,
                "{code} 行情 missing；即时成交 fail closed"
            ),
            RuleError::InstrumentSuspended { code } => write!(
                f,
                "{code} 停牌（tradeStatus=halted）；即时成交 fail closed"
            ),
            RuleError::OutsideTradingSessionQuote { code } => write!(
                f,
                "{code} 非交易时段或行情 tradeStatus=closed；即时成交 fail closed"
            ),
        }
    }
}

impl std::error::Error for RuleError {}

/// 业务错误统一类型——RuleError + IO 错误（DB / 行情 / 序列化）。
#[derive(Debug, Clone)]
pub enum AccountError {
    Rule(RuleError),
    Io(String),
}

impl fmt::Display for AccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccountError::Rule(r) => write!(f, "{r}"),
            AccountError::Io(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for AccountError {}

impl AccountError {
    /// 映射成 spec `shared-types.md §5 ErrorCode` 闭集合 —— 给 OperateAccountResult.reason 用。
    pub fn to_error_code(&self) -> crate::domain::shared::ErrorCode {
        use crate::domain::shared::ErrorCode;
        match self {
            AccountError::Rule(r) => match r {
                RuleError::InsufficientFunds { .. } => ErrorCode::InsufficientCash,
                RuleError::TPlusOneViolation { .. } => ErrorCode::InsufficientSellableQuantity,
                RuleError::OutsideTradingHours => ErrorCode::OutsideTradingSession,
                RuleError::NotFillable => ErrorCode::DepthMissing,
                RuleError::SharesNotIntegerLot { .. } => ErrorCode::InvalidLotSize,
                RuleError::InsufficientShares { .. } => ErrorCode::InsufficientSellableQuantity,
                RuleError::PositionNotFound(_) => ErrorCode::NotFound,
                RuleError::PositionAlreadyClosed(_) => ErrorCode::InvalidInput,
                RuleError::DuplicateOpenCode(_) => ErrorCode::InvalidInput,
                RuleError::InvalidStops(_) => ErrorCode::InvalidInput,
                RuleError::NoCurrentPrice(_) => ErrorCode::QuoteMissing,
                RuleError::InvalidCode(_) => ErrorCode::InvalidInput,
                RuleError::ScaleWouldZero => ErrorCode::InvalidInput,
                RuleError::QuoteStale { .. } => ErrorCode::QuoteStale,
                RuleError::QuoteMissing { .. } => ErrorCode::QuoteMissing,
                RuleError::InstrumentSuspended { .. } => ErrorCode::InstrumentSuspended,
                RuleError::OutsideTradingSessionQuote { .. } => ErrorCode::OutsideTradingSession,
            },
            AccountError::Io(_) => ErrorCode::DbError,
        }
    }
}

impl From<RuleError> for AccountError {
    fn from(r: RuleError) -> Self {
        AccountError::Rule(r)
    }
}

impl From<String> for AccountError {
    fn from(s: String) -> Self {
        AccountError::Io(s)
    }
}
