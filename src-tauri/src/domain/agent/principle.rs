//! Principle aggregate——结构化投资原则 / 已知偏差（归属 agent BC）。
//!
//! 见 docs/design/agent-redesign.md「关键概念：Principle」与 § 5.2 防膨胀防死循环。
//!
//! 关键设计：
//! - 每条 principle 是独立实体，带 origin / state / hit_count / regime_tags
//! - Reflection 写 proposed → ≥3 hit 或用户复述升 active
//! - origin=user_stated 的 principle agent 不能自己 +hit_count（防 RLHF reward hacking）
//! - 30 天未 hit → dormant；不再进 prompt 但保留
//! - prompt 注入按 hit_count 降序 + 当前 regime 过滤 + 上限 25 条

use crate::domain::quotes::regime::Regime;
use crate::domain::shared::OccurredAt;
use serde::{Deserialize, Serialize};

// ====== ID ==============================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PrincipleId(String);

impl PrincipleId {
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

impl std::fmt::Display for PrincipleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for PrincipleId {
    fn default() -> Self {
        Self::new()
    }
}

// ====== Aggregate =======================================================

/// 单条 principle 字数硬上限（spec § 5.6）。
pub const PRINCIPLE_BODY_MAX_CHARS: usize = 120;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Principle {
    pub id: PrincipleId,
    pub body: String,
    pub category: PrincipleCategory,
    pub origin: PrincipleOrigin,
    pub state: PrincipleState,
    /// 哪些市场状态下适用；空表示通用（all regimes）。
    pub regime_tags: Vec<Regime>,
    pub hit_count: u32,
    pub last_applied_at: Option<OccurredAt>,
    pub created_at: OccurredAt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipleCategory {
    /// 通用投资原则（"信息不足时观察 > 交易"）
    Principle,
    /// 已知偏差（"我倾向追涨"）
    KnownBias,
    /// 风险偏好（"单笔仓位 ≤ 5%"）
    RiskPreference,
}

impl PrincipleCategory {
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
pub enum PrincipleOrigin {
    /// 用户口头说的——hit_count 只能用户复述时 +1，agent 不能自己加。
    UserStated,
    /// Agent reflection 学到的——hit_count 可自然增减。
    AgentInferred,
}

impl PrincipleOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserStated => "user_stated",
            Self::AgentInferred => "agent_inferred",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user_stated" => Some(Self::UserStated),
            "agent_inferred" => Some(Self::AgentInferred),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipleState {
    /// 提议中——agent reflection 写入的初始态，未进 prompt。
    /// 升 Active 条件：≥3 hit（agent_inferred）或用户复述 / confirm（user_stated）。
    Proposed,
    /// 在用——进 prompt 注入候选池。
    Active,
    /// 沉睡——30 天未 hit，不进 prompt 但保留（可能某天 regime 变了又有用）。
    Dormant,
    /// 退役——软删除（agent / 用户主动淘汰）。
    Retired,
}

impl PrincipleState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Active => "active",
            Self::Dormant => "dormant",
            Self::Retired => "retired",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "proposed" => Some(Self::Proposed),
            "active" => Some(Self::Active),
            "dormant" => Some(Self::Dormant),
            "retired" => Some(Self::Retired),
            _ => None,
        }
    }

    /// 是否进 prompt 注入候选池。
    pub fn is_promptable(self) -> bool {
        matches!(self, Self::Active)
    }
}

// ====== 构造辅助 ========================================================

impl Principle {
    /// agent reflection 提议（state=proposed, origin=agent_inferred）。
    pub fn propose_by_agent(
        body: String,
        category: PrincipleCategory,
        regime_tags: Vec<Regime>,
        now: OccurredAt,
    ) -> Self {
        Self {
            id: PrincipleId::new(),
            body: truncate_body(&body),
            category,
            origin: PrincipleOrigin::AgentInferred,
            state: PrincipleState::Proposed,
            regime_tags,
            hit_count: 0,
            last_applied_at: None,
            created_at: now,
        }
    }

    /// 用户口头表达（origin=user_stated）。
    /// Phase 1 简化：用户直接说的 principle 一上来就是 active（user_stated 不需要 hit 验证）。
    pub fn from_user(
        body: String,
        category: PrincipleCategory,
        regime_tags: Vec<Regime>,
        now: OccurredAt,
    ) -> Self {
        Self {
            id: PrincipleId::new(),
            body: truncate_body(&body),
            category,
            origin: PrincipleOrigin::UserStated,
            state: PrincipleState::Active,
            regime_tags,
            hit_count: 0,
            last_applied_at: None,
            created_at: now,
        }
    }

    /// Seed principle（系统启动注入，origin=user_stated, state=active）。
    pub fn seed(
        body: String,
        category: PrincipleCategory,
        regime_tags: Vec<Regime>,
        now: OccurredAt,
    ) -> Self {
        Self::from_user(body, category, regime_tags, now)
    }
}

fn truncate_body(s: &str) -> String {
    let trimmed = s.trim();
    let mut out = String::new();
    let mut count = 0;
    for c in trimmed.chars() {
        if count >= PRINCIPLE_BODY_MAX_CHARS {
            break;
        }
        out.push(c);
        count += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_body_to_cap() {
        let long = "a".repeat(200);
        let now = OccurredAt::now();
        let p = Principle::seed(long, PrincipleCategory::Principle, vec![], now);
        assert_eq!(p.body.chars().count(), PRINCIPLE_BODY_MAX_CHARS);
    }

    #[test]
    fn user_stated_seed_is_active() {
        let now = OccurredAt::now();
        let p = Principle::seed("test".into(), PrincipleCategory::Principle, vec![], now);
        assert_eq!(p.state, PrincipleState::Active);
        assert_eq!(p.origin, PrincipleOrigin::UserStated);
    }

    #[test]
    fn agent_proposal_is_proposed_state() {
        let now = OccurredAt::now();
        let p = Principle::propose_by_agent(
            "test".into(),
            PrincipleCategory::Principle,
            vec![Regime::Bull],
            now,
        );
        assert_eq!(p.state, PrincipleState::Proposed);
        assert_eq!(p.origin, PrincipleOrigin::AgentInferred);
    }
}
