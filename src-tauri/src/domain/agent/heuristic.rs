//! Heuristic aggregate——结构化启发式规则 + 实战 track record。
//!
//! 见 docs/design/agent-v3-expectation-driven.md § 2.1 (heuristics 表) + § 8.3。
//!
//! 取代 v2 的 Principle（语义升级）。差异：
//! - 带 `supporting_lesson_ids` —— 必须基于已发生的 lessons emerge，不能凭空写
//! - 带 `application_count / hit_count / miss_count` —— 实战 track record
//! - 带 `confidence()` 派生方法 —— 连续值，替代 v2 二态 proposed/active
//! - 加 `origin = seed` —— Phase 1 启动注入的人写原则归类
//! - state 不存——`effective_state()` 由 confidence + retired_at 派生
//!
//! v3 学习闭环核心：lessons emerge → heuristic 创建 → 进 prompt → 被应用 →
//! expectation review 反向标 hit/miss → confidence 更新 → 影响下次是否进 prompt。

use crate::domain::agent::lesson::LessonId;
use crate::domain::quotes::regime::Regime;
use crate::domain::shared::OccurredAt;
use serde::{Deserialize, Serialize};

// ====== ID ==============================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HeuristicId(String);

impl HeuristicId {
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

impl std::fmt::Display for HeuristicId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for HeuristicId {
    fn default() -> Self {
        Self::new()
    }
}

// ====== Category / Origin ===============================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeuristicCategory {
    /// 通用投资原则（"信息不足时观察 > 交易"）
    Principle,
    /// 已知偏差（"我倾向追涨"）
    KnownBias,
    /// 风险偏好（"单笔仓位 ≤ 5%"）
    RiskPreference,
}

impl HeuristicCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Principle => "principle",
            Self::KnownBias => "known_bias",
            Self::RiskPreference => "risk_preference",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "principle" => Some(Self::Principle),
            "known_bias" => Some(Self::KnownBias),
            "risk_preference" => Some(Self::RiskPreference),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeuristicOrigin {
    /// 系统启动时手写 seed（永远 active，参与 confidence 计算特殊处理）
    Seed,
    /// 用户口头说出来——直接 active；agent 不能为其加 hit_count
    UserStated,
    /// agent reflection 从 lessons emerge 出来——按 confidence 决定生效
    AgentInferred,
}

impl HeuristicOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Seed => "seed",
            Self::UserStated => "user_stated",
            Self::AgentInferred => "agent_inferred",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "seed" => Some(Self::Seed),
            "user_stated" => Some(Self::UserStated),
            "agent_inferred" => Some(Self::AgentInferred),
            _ => None,
        }
    }
    /// origin=user_stated/seed 的 heuristic 不接受系统自动 hit_count 累加。
    /// 仅当用户在 chat 显式复述、或 LLM 主动调 record_heuristic_feedback 时才计数。
    pub fn allows_system_hit_count(self) -> bool {
        matches!(self, Self::AgentInferred)
    }
    /// origin=seed/user_stated 永远进 prompt；origin=agent_inferred 按 confidence 过滤。
    pub fn unconditionally_active(self) -> bool {
        matches!(self, Self::Seed | Self::UserStated)
    }
}

// ====== Effective state（confidence + retired_at 派生）===================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveState {
    /// 进 prompt 主候选（high confidence 或 seed/user_stated）
    Active,
    /// 进 prompt 但带 ⚠️ 标记（confidence 中段）
    Challenged,
    /// 进 prompt 带"未充分验证"标记（样本不足）
    Probationary,
    /// 不进 prompt（confidence 太低或 30+ 天未应用）
    Dormant,
    /// 软删除
    Retired,
}

impl EffectiveState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Challenged => "challenged",
            Self::Probationary => "probationary",
            Self::Dormant => "dormant",
            Self::Retired => "retired",
        }
    }
    pub fn is_promptable(self) -> bool {
        matches!(
            self,
            Self::Active | Self::Challenged | Self::Probationary
        )
    }
}

// ====== Aggregate =======================================================

/// 单条 heuristic body 字数硬上限。
pub const HEURISTIC_BODY_MAX_CHARS: usize = 120;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Heuristic {
    pub id: HeuristicId,
    pub body: String,
    pub category: HeuristicCategory,
    pub origin: HeuristicOrigin,
    /// 适用市场状态——空表示通用（all regimes）
    pub regime_tags: Vec<Regime>,
    /// 支持本 heuristic 的 lessons（origin=AgentInferred 时必填非空；Seed/UserStated 可空）
    pub supporting_lesson_ids: Vec<LessonId>,
    /// 累计被 expectation review 计数到的次数（仅 origin=AgentInferred）
    pub application_count: u32,
    pub hit_count: u32,
    pub miss_count: u32,
    pub last_applied_at: Option<OccurredAt>,
    pub retired_at: Option<OccurredAt>,
    pub retired_reason: Option<String>,
    pub created_at: OccurredAt,
}

impl Heuristic {
    /// 命中率（仅 application_count ≥ 3 才返回 Some；样本太少不算 confidence）
    pub fn confidence(&self) -> Option<f32> {
        let total = self.hit_count + self.miss_count;
        if total < 3 {
            return None;
        }
        Some(self.hit_count as f32 / total as f32)
    }

    /// 派生 effective state：confidence + retired_at + origin 综合判定。
    pub fn effective_state(&self) -> EffectiveState {
        if self.retired_at.is_some() {
            return EffectiveState::Retired;
        }
        // seed / user_stated 永远 active
        if self.origin.unconditionally_active() {
            return EffectiveState::Active;
        }
        // agent_inferred 按 confidence 派生
        match self.confidence() {
            Some(c) if c >= 0.6 => EffectiveState::Active,
            Some(c) if c >= 0.3 => EffectiveState::Challenged,
            Some(_) => EffectiveState::Dormant,
            None => EffectiveState::Probationary, // 样本不足但仍展示给 LLM
        }
    }

    pub fn is_promptable(&self) -> bool {
        self.effective_state().is_promptable()
    }
}

fn truncate_body(s: &str) -> String {
    let trimmed = s.trim();
    let mut out = String::new();
    let mut count = 0;
    for c in trimmed.chars() {
        if count >= HEURISTIC_BODY_MAX_CHARS {
            break;
        }
        out.push(c);
        count += 1;
    }
    out
}

// ====== 构造辅助 ========================================================

impl Heuristic {
    /// 系统启动 seed 注入（用户写好的原则）
    pub fn seed(body: String, category: HeuristicCategory, now: OccurredAt) -> Self {
        Self {
            id: HeuristicId::new(),
            body: truncate_body(&body),
            category,
            origin: HeuristicOrigin::Seed,
            regime_tags: Vec::new(),
            supporting_lesson_ids: Vec::new(),
            application_count: 0,
            hit_count: 0,
            miss_count: 0,
            last_applied_at: None,
            retired_at: None,
            retired_reason: None,
            created_at: now,
        }
    }

    /// 用户在 chat 显式声明的偏好——直接 active，无需 supporting_lessons
    pub fn from_user(
        body: String,
        category: HeuristicCategory,
        regime_tags: Vec<Regime>,
        now: OccurredAt,
    ) -> Self {
        Self {
            id: HeuristicId::new(),
            body: truncate_body(&body),
            category,
            origin: HeuristicOrigin::UserStated,
            regime_tags,
            supporting_lesson_ids: Vec::new(),
            application_count: 0,
            hit_count: 0,
            miss_count: 0,
            last_applied_at: None,
            retired_at: None,
            retired_reason: None,
            created_at: now,
        }
    }

    /// agent reflection emerge——必须基于 ≥1 lesson
    pub fn emerge_from_lessons(
        body: String,
        category: HeuristicCategory,
        regime_tags: Vec<Regime>,
        supporting_lesson_ids: Vec<LessonId>,
        now: OccurredAt,
    ) -> Result<Self, String> {
        if supporting_lesson_ids.is_empty() {
            return Err("agent_inferred heuristic 必须基于 ≥1 lesson".into());
        }
        Ok(Self {
            id: HeuristicId::new(),
            body: truncate_body(&body),
            category,
            origin: HeuristicOrigin::AgentInferred,
            regime_tags,
            supporting_lesson_ids,
            application_count: 0,
            hit_count: 0,
            miss_count: 0,
            last_applied_at: None,
            retired_at: None,
            retired_reason: None,
            created_at: now,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_is_unconditionally_active() {
        let h = Heuristic::seed(
            "信息不足时观察 > 交易".into(),
            HeuristicCategory::Principle,
            OccurredAt::new(0),
        );
        assert_eq!(h.effective_state(), EffectiveState::Active);
        assert!(h.is_promptable());
    }

    #[test]
    fn user_stated_is_unconditionally_active() {
        let h = Heuristic::from_user(
            "不追涨停".into(),
            HeuristicCategory::KnownBias,
            vec![],
            OccurredAt::new(0),
        );
        assert_eq!(h.effective_state(), EffectiveState::Active);
    }

    #[test]
    fn agent_inferred_probationary_when_no_samples() {
        let h = Heuristic::emerge_from_lessons(
            "x".into(),
            HeuristicCategory::Principle,
            vec![],
            vec![LessonId::new()],
            OccurredAt::new(0),
        )
        .unwrap();
        assert_eq!(h.effective_state(), EffectiveState::Probationary);
        assert!(h.is_promptable()); // probationary 仍进 prompt
    }

    #[test]
    fn agent_inferred_active_at_high_confidence() {
        let mut h = Heuristic::emerge_from_lessons(
            "x".into(),
            HeuristicCategory::Principle,
            vec![],
            vec![LessonId::new()],
            OccurredAt::new(0),
        )
        .unwrap();
        h.hit_count = 8;
        h.miss_count = 2;
        assert_eq!(h.effective_state(), EffectiveState::Active);
    }

    #[test]
    fn agent_inferred_challenged_at_mid_confidence() {
        let mut h = Heuristic::emerge_from_lessons(
            "x".into(),
            HeuristicCategory::Principle,
            vec![],
            vec![LessonId::new()],
            OccurredAt::new(0),
        )
        .unwrap();
        h.hit_count = 4;
        h.miss_count = 6;
        // 0.4 → challenged
        assert_eq!(h.effective_state(), EffectiveState::Challenged);
    }

    #[test]
    fn agent_inferred_dormant_at_low_confidence() {
        let mut h = Heuristic::emerge_from_lessons(
            "x".into(),
            HeuristicCategory::Principle,
            vec![],
            vec![LessonId::new()],
            OccurredAt::new(0),
        )
        .unwrap();
        h.hit_count = 1;
        h.miss_count = 9;
        // 0.1 → dormant
        assert_eq!(h.effective_state(), EffectiveState::Dormant);
        assert!(!h.is_promptable());
    }

    #[test]
    fn retired_overrides_everything() {
        let mut h = Heuristic::seed(
            "x".into(),
            HeuristicCategory::Principle,
            OccurredAt::new(0),
        );
        h.retired_at = Some(OccurredAt::new(1));
        assert_eq!(h.effective_state(), EffectiveState::Retired);
        assert!(!h.is_promptable());
    }

    #[test]
    fn agent_inferred_requires_supporting_lessons() {
        let result = Heuristic::emerge_from_lessons(
            "x".into(),
            HeuristicCategory::Principle,
            vec![],
            vec![],
            OccurredAt::new(0),
        );
        assert!(result.is_err());
    }

    #[test]
    fn origin_allows_system_hit_count() {
        assert!(HeuristicOrigin::AgentInferred.allows_system_hit_count());
        assert!(!HeuristicOrigin::UserStated.allows_system_hit_count());
        assert!(!HeuristicOrigin::Seed.allows_system_hit_count());
    }

    #[test]
    fn body_truncated_to_max_chars() {
        let long = "a".repeat(200);
        let h = Heuristic::seed(long, HeuristicCategory::Principle, OccurredAt::new(0));
        assert_eq!(h.body.chars().count(), HEURISTIC_BODY_MAX_CHARS);
    }
}
