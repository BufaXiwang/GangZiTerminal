//! Lesson aggregate——每个 Expectation 终态时自动生成的原子观察。
//!
//! 见 docs/design/agent-v3-expectation-driven.md § 8.1。
//!
//! Lesson 是学习闭环的**最底层原料**——不允许凭空写，只能从已发生的
//! expectation outcome 派生。≥2 条共有模式的 lessons 会 emerge 成 Heuristic。
//!
//! 设计原则：
//! - observation：客观事实，由代码生成（"在 X 价开仓 Y 天后 Z 价平 盈亏 N%"）
//! - takeaway：可学习的一句话教训，由 reflection 时 LLM 写
//! - 永不修改、永不删除——历史数据完整保留

use crate::domain::account::expectation::ExpectationId;
use crate::domain::shared::signal::SignalKind;
use crate::domain::quotes::regime::Regime;
use crate::domain::shared::{OccurredAt, StockCode};
use serde::{Deserialize, Serialize};

// ====== ID ==============================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LessonId(String);

impl LessonId {
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

impl std::fmt::Display for LessonId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for LessonId {
    fn default() -> Self {
        Self::new()
    }
}

// ====== Outcome ========================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LessonOutcome {
    /// expectation 命中
    Hit,
    /// 到期未达 target
    Miss,
    /// 观察型到期或区间预期到期（既未命中也未明确证伪节奏）
    Expired,
}

impl LessonOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Expired => "expired",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hit" => Some(Self::Hit),
            "miss" => Some(Self::Miss),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

// ====== Aggregate =======================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lesson {
    pub id: LessonId,
    pub expectation_id: ExpectationId,
    pub code: StockCode,
    /// 客观事实（代码生成）："在 X 价开仓 Y 天后 Z 价平 盈亏 N%"
    pub observation: String,
    /// 可学习一句话教训（LLM 生成）："ST 板块涨停日的回踩通常是诱多"
    pub takeaway: String,
    pub outcome: LessonOutcome,
    pub regime_at_close: Option<Regime>,
    /// 该 expectation 触发用的 signals——emerge heuristic 时聚类用
    pub signals_in_play: Vec<SignalKind>,
    /// 关联持仓的盈亏百分比（可选——纯观察型 expectation 无持仓时为 None）
    pub pnl_pct: Option<f64>,
    pub created_at: OccurredAt,
}

impl Lesson {
    pub fn new(
        expectation_id: ExpectationId,
        code: StockCode,
        observation: String,
        takeaway: String,
        outcome: LessonOutcome,
        regime: Option<Regime>,
        signals_in_play: Vec<SignalKind>,
        pnl_pct: Option<f64>,
        now: OccurredAt,
    ) -> Self {
        Self {
            id: LessonId::new(),
            expectation_id,
            code,
            observation,
            takeaway,
            outcome,
            regime_at_close: regime,
            signals_in_play,
            pnl_pct,
            created_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_round_trip() {
        for o in [LessonOutcome::Hit, LessonOutcome::Miss, LessonOutcome::Expired] {
            let s = o.as_str();
            assert_eq!(LessonOutcome::parse(s), Some(o));
        }
    }
}
