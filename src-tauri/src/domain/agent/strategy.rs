//! Strategy DSL——用户 + agent 共建的"什么时候建 Expectation"规则集。
//!
//! 见 docs/design/agent-v3-expectation-driven.md § 4。
//!
//! 每个 Strategy 是一组 trigger_when 条件 + target 推导规则。当 watchlist
//! 上的某只股票满足 Strategy.trigger_when 时，scan 阶段调 LLM 决定是否真的建仓。
//! Strategy 自带 applied_count / hit_count / miss_count 用于反向打分 strategy 本身。

use crate::domain::account::expectation::Direction;
use crate::domain::shared::signal::SignalKind;
use crate::domain::shared::OccurredAt;
use serde::{Deserialize, Serialize};

// ====== ID ==============================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StrategyId(String);

impl StrategyId {
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

impl std::fmt::Display for StrategyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for StrategyId {
    fn default() -> Self {
        Self::new()
    }
}

// ====== 触发条件 / 目标规则 / 把握度规则 =================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TriggerLogic {
    /// 所有 condition 都必须满足
    And,
    /// 任一 condition 满足即可（不推荐 Phase 1）
    Or,
}

impl Default for TriggerLogic {
    fn default() -> Self {
        Self::And
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalCondition {
    /// 必须命中的信号（按 family 匹配，参数仅作为提示给 LLM 看）
    pub signal: SignalKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetRule {
    pub direction: Direction,
    /// 目标价相对当前价的百分点（Up=正、Down=负也行，绝对值取）
    pub pct_relative_to_current: f32,
    pub horizon_days: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConvictionRule {
    /// 触发以下任一附加 signal 时升级到 high
    pub high_if: Vec<SignalKind>,
    /// 兜底 conviction（默认 Medium）
    #[serde(default)]
    pub medium_default: bool,
}

impl Default for ConvictionRule {
    fn default() -> Self {
        Self {
            high_if: Vec::new(),
            medium_default: true,
        }
    }
}

// ====== Strategy Aggregate ==============================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Strategy {
    pub id: StrategyId,
    pub name: String,
    pub description: String,
    /// 触发条件列表（DSL 主体）
    pub trigger_when: Vec<SignalCondition>,
    #[serde(default)]
    pub trigger_logic: TriggerLogic,
    pub target: TargetRule,
    #[serde(default)]
    pub conviction_rule: ConvictionRule,
    pub enabled: bool,
    /// 该 strategy 累计触发次数（每次 trigger_when 命中并 LLM 决定建仓）
    pub applied_count: u32,
    /// 该 strategy 触发后形成的 expectation 命中数 / 错过数
    pub hit_count: u32,
    pub miss_count: u32,
    pub created_at: OccurredAt,
    pub updated_at: OccurredAt,
}

impl Strategy {
    pub fn confidence(&self) -> Option<f32> {
        let total = self.hit_count + self.miss_count;
        if total < 3 {
            return None;
        }
        Some(self.hit_count as f32 / total as f32)
    }
}

// ====== Strategy events（用户/agent 修改时审计）==========================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StrategyEvent {
    Created,
    Updated { changes: serde_json::Value, reason: String },
    Enabled,
    Disabled { reason: String },
    UserComment { text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StrategyEventRecord {
    pub strategy_id: StrategyId,
    pub event: StrategyEvent,
    pub occurred_at: OccurredAt,
}

// ====== 构造辅助 ========================================================

impl Strategy {
    pub fn new(
        name: String,
        description: String,
        trigger_when: Vec<SignalCondition>,
        target: TargetRule,
        now: OccurredAt,
    ) -> Self {
        Self {
            id: StrategyId::new(),
            name,
            description,
            trigger_when,
            trigger_logic: TriggerLogic::And,
            target,
            conviction_rule: ConvictionRule::default(),
            enabled: true,
            applied_count: 0,
            hit_count: 0,
            miss_count: 0,
            created_at: now,
            updated_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shared::OccurredAt;

    #[test]
    fn default_strategy_is_enabled_and_zero_track() {
        let s = Strategy::new(
            "动量突破".into(),
            "test".into(),
            vec![SignalCondition {
                signal: SignalKind::BreakoutAbove20MA,
            }],
            TargetRule {
                direction: Direction::Up,
                pct_relative_to_current: 7.0,
                horizon_days: 8,
            },
            OccurredAt::new(0),
        );
        assert!(s.enabled);
        assert_eq!(s.applied_count, 0);
        assert!(s.confidence().is_none()); // 样本不足
    }

    #[test]
    fn confidence_kicks_in_after_3_samples() {
        let mut s = Strategy::new(
            "x".into(),
            "y".into(),
            vec![],
            TargetRule {
                direction: Direction::Up,
                pct_relative_to_current: 5.0,
                horizon_days: 5,
            },
            OccurredAt::new(0),
        );
        s.hit_count = 2;
        s.miss_count = 0;
        assert!(s.confidence().is_none()); // 2 < 3
        s.hit_count = 3;
        assert_eq!(s.confidence(), Some(1.0));
        s.miss_count = 1;
        assert_eq!(s.confidence(), Some(0.75));
    }
}
