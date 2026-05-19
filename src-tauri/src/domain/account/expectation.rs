//! Expectation aggregate——投资预期一等公民（归属 account BC）。
//!
//! 见 docs/design/agent-v3-expectation-driven.md 关键概念 / § 2.1。
//!
//! 一个 Expectation = 一次可量化、可代码自动验证的押注：
//! - code + direction + target_price + horizon_days：核心预测
//! - signals_used：触发本预期的结构化信号列表（驱动 hit/miss 时反向打标）
//! - reasoning：叙事 / 决策上下文（原 v2 Thesis.hypothesis 的角色）
//! - theme：跨股聚合标签（"光模块算力" / "新能源补涨"）
//! - supersedes：链向上一个 expectation，形成滚动跟踪时间序列
//! - 状态机：pending → hit / missed / expired / cancelled / superseded
//!
//! 自动 review：每个 tick / 盘后 reflection 拿 quote 算 `judge_outcome`，纯函数判定。

use crate::domain::shared::signal::SignalKind;
use crate::domain::quotes::regime::Regime;
use crate::domain::shared::{OccurredAt, StockCode, Yuan};
use serde::{Deserialize, Serialize};

/// 预期把握度——影响仓位 sizing 和 strategy 标记升级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conviction {
    Low,
    Medium,
    High,
}

impl Conviction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

// ====== ID ==============================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExpectationId(String);

impl ExpectationId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
    pub fn from_string(s: String) -> Self {
        Self(s)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ExpectationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for ExpectationId {
    fn default() -> Self {
        Self::new()
    }
}

// ====== Direction =======================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// 看涨：到/破 target_price 上方
    Up,
    /// 看跌：到/破 target_price 下方
    Down,
    /// 区间震荡：价格保持在 [target_price, target_price_ceiling] 内
    RangeBound,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::RangeBound => "range_bound",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            "range_bound" => Some(Self::RangeBound),
            _ => None,
        }
    }
}

// ====== State 状态机 ====================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpectationState {
    /// 等待 target 触达或 horizon 到期
    Pending,
    /// 命中（横坐标可在 expires 之前到价）
    Hit,
    /// 到期未触达 target
    Missed,
    /// 到期但 direction=RangeBound 时算 hit；其他 direction 等同 missed
    /// 单独 expired 态用于"horizon 到期且无 target_price（仅观察型）"
    Expired,
    /// 用户 / agent 主动撤
    Cancelled,
    /// 被后续 expectation supersedes
    Superseded,
}

impl ExpectationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Hit => "hit",
            Self::Missed => "missed",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
            Self::Superseded => "superseded",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "hit" => Some(Self::Hit),
            "missed" => Some(Self::Missed),
            "expired" => Some(Self::Expired),
            "cancelled" => Some(Self::Cancelled),
            "superseded" => Some(Self::Superseded),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
    }

    /// hit/miss 累积到关联 signals 的 heuristics 时调；expired/cancelled/superseded 不计入。
    pub fn counts_for_signal_outcome(self) -> Option<bool> {
        match self {
            Self::Hit => Some(true),
            Self::Missed => Some(false),
            _ => None,
        }
    }
}

// ====== Aggregate =======================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Expectation {
    pub id: ExpectationId,
    pub code: StockCode,
    pub direction: Direction,
    /// 目标价（Down 时是下沿，Up 时是上沿，RangeBound 时是区间下沿）
    /// None 表示纯观察型（"看好但还没量化目标"）
    pub target_price: Option<Yuan>,
    /// 区间预期的上沿（仅 RangeBound 使用）
    pub target_price_ceiling: Option<Yuan>,
    pub horizon_days: u32,
    /// 自然语言决策上下文——叙事 / 这次为什么押
    pub reasoning: String,
    /// 触发本预期的结构化信号列表——hit/miss 时反向打标
    pub signals_used: Vec<SignalKind>,
    pub conviction: Conviction,
    /// 跨股聚合标签（"光模块算力" / null）
    pub theme: Option<String>,
    /// 链向上一个 expectation——形成滚动跟踪时间序列
    pub supersedes: Option<ExpectationId>,
    pub state: ExpectationState,
    pub regime_at_creation: Option<Regime>,
    pub created_at: OccurredAt,
    pub expires_at: OccurredAt,
    pub closed_at: Option<OccurredAt>,
}

// ====== Event 链（append-only 状态机审计） ===============================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExpectationEvent {
    Created,
    Hit {
        actual_price: Yuan,
        reason: String,
    },
    Missed {
        actual_price: Yuan,
        reason: String,
    },
    Expired {
        reason: String,
    },
    Cancelled {
        reason: String,
    },
    Superseded {
        by: ExpectationId,
    },
    UserFeedback {
        text: String,
    },
    Note {
        text: String,
    },
}

impl ExpectationEvent {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Hit { .. } => "hit",
            Self::Missed { .. } => "missed",
            Self::Expired { .. } => "expired",
            Self::Cancelled { .. } => "cancelled",
            Self::Superseded { .. } => "superseded",
            Self::UserFeedback { .. } => "user_feedback",
            Self::Note { .. } => "note",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectationEventRecord {
    pub expectation_id: ExpectationId,
    pub event: ExpectationEvent,
    pub occurred_at: OccurredAt,
}

// ====== judge_outcome 纯函数 ============================================

/// review 时调用——根据当前 quote + 时间，决定 expectation 应推进到哪个 state。
///
/// 不修改 expectation；仅返回建议的下个 state + 实际触达价（如有）。
#[derive(Debug, Clone)]
pub enum OutcomeJudgment {
    StillPending,
    Hit {
        actual_price: Yuan,
        reason: String,
    },
    Missed {
        actual_price: Yuan,
        reason: String,
    },
    Expired {
        reason: String,
    },
}

pub fn judge_outcome(
    exp: &Expectation,
    current_price: Yuan,
    now: OccurredAt,
) -> OutcomeJudgment {
    let expired = now.value() >= exp.expires_at.value();

    // 无 target_price：仅观察型——到期即 expired，否则 pending
    let Some(target) = exp.target_price else {
        return if expired {
            OutcomeJudgment::Expired {
                reason: "horizon 到期且为观察型 expectation（无 target_price）".into(),
            }
        } else {
            OutcomeJudgment::StillPending
        };
    };

    // 有 target_price：根据 direction 判定
    match exp.direction {
        Direction::Up => {
            if current_price.value() >= target.value() {
                OutcomeJudgment::Hit {
                    actual_price: current_price,
                    reason: format!(
                        "current {} 已达/超 up target {}",
                        current_price.value(),
                        target.value()
                    ),
                }
            } else if expired {
                OutcomeJudgment::Missed {
                    actual_price: current_price,
                    reason: format!(
                        "到期 current {} 未达 up target {}",
                        current_price.value(),
                        target.value()
                    ),
                }
            } else {
                OutcomeJudgment::StillPending
            }
        }
        Direction::Down => {
            if current_price.value() <= target.value() {
                OutcomeJudgment::Hit {
                    actual_price: current_price,
                    reason: format!(
                        "current {} 已达/破 down target {}",
                        current_price.value(),
                        target.value()
                    ),
                }
            } else if expired {
                OutcomeJudgment::Missed {
                    actual_price: current_price,
                    reason: format!(
                        "到期 current {} 未破 down target {}",
                        current_price.value(),
                        target.value()
                    ),
                }
            } else {
                OutcomeJudgment::StillPending
            }
        }
        Direction::RangeBound => {
            // 区间预期：当前价必须在 [target_price, target_price_ceiling] 内才算 hit
            let ceiling = exp.target_price_ceiling.unwrap_or(target);
            let lower = target.value();
            let upper = ceiling.value();
            let inside = current_price.value() >= lower && current_price.value() <= upper;
            if expired {
                if inside {
                    OutcomeJudgment::Hit {
                        actual_price: current_price,
                        reason: format!(
                            "到期 current {} 在 [{}, {}] 区间内",
                            current_price.value(),
                            lower,
                            upper
                        ),
                    }
                } else {
                    OutcomeJudgment::Missed {
                        actual_price: current_price,
                        reason: format!(
                            "到期 current {} 跌出 [{}, {}] 区间",
                            current_price.value(),
                            lower,
                            upper
                        ),
                    }
                }
            } else if !inside {
                // 提前破区间也算 missed
                OutcomeJudgment::Missed {
                    actual_price: current_price,
                    reason: format!(
                        "current {} 提前破 [{}, {}] 区间",
                        current_price.value(),
                        lower,
                        upper
                    ),
                }
            } else {
                OutcomeJudgment::StillPending
            }
        }
    }
}

// ====== 构造辅助 ========================================================

impl Expectation {
    pub fn create(
        code: StockCode,
        direction: Direction,
        target_price: Option<Yuan>,
        target_price_ceiling: Option<Yuan>,
        horizon_days: u32,
        reasoning: String,
        signals_used: Vec<SignalKind>,
        conviction: Conviction,
        theme: Option<String>,
        supersedes: Option<ExpectationId>,
        regime: Option<Regime>,
        now: OccurredAt,
        expires_at: OccurredAt,
    ) -> Self {
        Self {
            id: ExpectationId::new(),
            code,
            direction,
            target_price,
            target_price_ceiling,
            horizon_days,
            reasoning,
            signals_used,
            conviction,
            theme,
            supersedes,
            state: ExpectationState::Pending,
            regime_at_creation: regime,
            created_at: now,
            expires_at,
            closed_at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shared::OccurredAt;

    fn make_expectation(
        direction: Direction,
        target: Option<Yuan>,
        expires_offset_ms: i64,
    ) -> Expectation {
        let now = OccurredAt::new(1_700_000_000_000);
        let expires = OccurredAt::new(1_700_000_000_000 + expires_offset_ms);
        Expectation::create(
            StockCode::new("600519").unwrap(),
            direction,
            target,
            None,
            5,
            "test".into(),
            vec![],
            Conviction::Medium,
            None,
            None,
            None,
            now,
            expires,
        )
    }

    #[test]
    fn judge_up_hit_when_price_reaches_target() {
        let exp = make_expectation(Direction::Up, Some(Yuan::new(110.0).unwrap()), 86_400_000);
        let now = OccurredAt::new(1_700_000_000_000 + 1000);
        let outcome = judge_outcome(&exp, Yuan::new(112.0).unwrap(), now);
        assert!(matches!(outcome, OutcomeJudgment::Hit { .. }));
    }

    #[test]
    fn judge_up_pending_when_below_target_and_not_expired() {
        let exp = make_expectation(Direction::Up, Some(Yuan::new(110.0).unwrap()), 86_400_000);
        let now = OccurredAt::new(1_700_000_000_000 + 1000);
        let outcome = judge_outcome(&exp, Yuan::new(105.0).unwrap(), now);
        assert!(matches!(outcome, OutcomeJudgment::StillPending));
    }

    #[test]
    fn judge_up_missed_when_expired_below_target() {
        let exp = make_expectation(Direction::Up, Some(Yuan::new(110.0).unwrap()), 86_400_000);
        let now = OccurredAt::new(1_700_000_000_000 + 86_400_001);
        let outcome = judge_outcome(&exp, Yuan::new(108.0).unwrap(), now);
        assert!(matches!(outcome, OutcomeJudgment::Missed { .. }));
    }

    #[test]
    fn judge_down_hit_when_price_drops_to_target() {
        let exp = make_expectation(Direction::Down, Some(Yuan::new(90.0).unwrap()), 86_400_000);
        let now = OccurredAt::new(1_700_000_000_000 + 1000);
        let outcome = judge_outcome(&exp, Yuan::new(88.0).unwrap(), now);
        assert!(matches!(outcome, OutcomeJudgment::Hit { .. }));
    }

    #[test]
    fn judge_observe_only_expires_at_horizon() {
        let exp = make_expectation(Direction::Up, None, 86_400_000);
        let now_expired = OccurredAt::new(1_700_000_000_000 + 86_400_001);
        let outcome = judge_outcome(&exp, Yuan::new(100.0).unwrap(), now_expired);
        assert!(matches!(outcome, OutcomeJudgment::Expired { .. }));
    }

    #[test]
    fn judge_range_bound_missed_when_breaks_below() {
        let mut exp = make_expectation(Direction::RangeBound, Some(Yuan::new(95.0).unwrap()), 86_400_000);
        exp.target_price_ceiling = Some(Yuan::new(105.0).unwrap());
        let now = OccurredAt::new(1_700_000_000_000 + 1000);
        let outcome = judge_outcome(&exp, Yuan::new(90.0).unwrap(), now);
        assert!(matches!(outcome, OutcomeJudgment::Missed { .. }));
    }

    #[test]
    fn state_terminal_check() {
        assert!(!ExpectationState::Pending.is_terminal());
        assert!(ExpectationState::Hit.is_terminal());
        assert!(ExpectationState::Missed.is_terminal());
        assert_eq!(ExpectationState::Hit.counts_for_signal_outcome(), Some(true));
        assert_eq!(ExpectationState::Missed.counts_for_signal_outcome(), Some(false));
        assert_eq!(ExpectationState::Expired.counts_for_signal_outcome(), None);
    }
}
