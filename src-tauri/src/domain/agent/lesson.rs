//! Lesson aggregate——每个 Position close 时自动生成的原子观察。
//!
//! Lesson 是学习闭环的**最底层原料**——不允许凭空写，只能从已发生的
//! position close outcome 派生。≥2 条共有模式的 lessons 会 emerge 成 Heuristic。
//!
//! 设计原则：
//! - observation：客观事实，由代码生成（"在 X 价开仓 Y 天后 Z 价平 盈亏 N%"）
//! - takeaway：可学习的一句话教训，由 reflection 时 LLM 写
//! - 永不修改、永不删除——历史数据完整保留

use crate::domain::account::position::PositionId;
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
    /// position close(TakeProfit) → 假设命中
    Hit,
    /// close(TimeStop) 且方向对未达 target——"目标定高了 / 节奏慢"
    PartialHit,
    /// close(StopLoss / Invalidated) 或 TimeStop 反向 → 假设破
    Miss,
    /// 观察型到期且无 target_price（既未命中也未明确证伪节奏）
    Expired,
}

impl LessonOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::PartialHit => "partial_hit",
            Self::Miss => "miss",
            Self::Expired => "expired",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hit" => Some(Self::Hit),
            "partial_hit" => Some(Self::PartialHit),
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
    pub position_id: PositionId,
    pub code: StockCode,
    /// 客观事实（代码生成）："在 X 价开仓 Y 天后 Z 价平 盈亏 N%"
    pub observation: String,
    /// 可学习一句话教训（LLM 生成）："ST 板块涨停日的回踩通常是诱多"
    pub takeaway: String,
    pub outcome: LessonOutcome,
    pub regime_at_close: Option<Regime>,
    /// 该 position 入场触发的 signals——emerge heuristic 时聚类用
    pub signals_in_play: Vec<SignalKind>,
    /// 关联持仓的盈亏百分比（Watch 类型为 None）
    pub pnl_pct: Option<f64>,
    pub created_at: OccurredAt,
}

impl Lesson {
    pub fn new(
        position_id: PositionId,
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
            position_id,
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
        for o in [
            LessonOutcome::Hit,
            LessonOutcome::PartialHit,
            LessonOutcome::Miss,
            LessonOutcome::Expired,
        ] {
            let s = o.as_str();
            assert_eq!(LessonOutcome::parse(s), Some(o));
        }
    }
}
