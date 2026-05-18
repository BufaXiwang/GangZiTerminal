//! Thesis aggregate——投资论点一等公民（归属 account BC）。
//!
//! 见 docs/design/agent-redesign.md 顶部「关键概念：Thesis 是什么」。
//!
//! 升一等理由（相对于旧的 PositionEvent.opened.payload.thesis: String）：
//! - 可以无持仓存在（"在跟踪但没建仓"是合法状态）
//! - 可以对应多只股票（1 thesis → N positions）
//! - 可以独立于持仓被证伪（invalidation 触发 → 自动平所有关联仓位）
//! - 是 reflection 的核心对象（复盘"论点错在哪"而不是"为什么平某只"）
//! - 状态机可查询（"想法现在还成立吗"是 SQL 可查事实）
//!
//! 归属 account 而非 agent：thesis 本质是"持仓的理由"，agent 只是主要作者非所有者。
//! 这保持 account 不感知 agent 的 DDD 边界。

use crate::domain::quotes::regime::Regime;
use crate::domain::shared::{OccurredAt, StockCode};
use serde::{Deserialize, Serialize};

// ====== ID ==============================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThesisId(String);

impl ThesisId {
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

impl std::fmt::Display for ThesisId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for ThesisId {
    fn default() -> Self {
        Self::new()
    }
}

// ====== Aggregate =======================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thesis {
    pub id: ThesisId,
    /// 核心论点——你赌的是什么发生。
    pub hypothesis: String,
    /// 失效条件——什么情况一出现就证伪，必须撤。
    pub invalidation: String,
    /// 验证清单——盯哪些指标 / 事件能确认论点在兑现。
    pub validation_checks: Vec<String>,
    pub conviction: Conviction,
    pub state: ThesisState,
    /// 一个 thesis 可对应多只股票（同一逻辑分仓）。
    pub target_codes: Vec<StockCode>,
    /// 创建时市场状态——复盘判断"这是哪种 regime 下做的判断"。
    pub regime_at_creation: Option<Regime>,
    pub created_at: OccurredAt,
    pub updated_at: OccurredAt,
    pub closed_at: Option<OccurredAt>,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThesisState {
    /// 起草中——agent 在写但未激活，不影响决策
    Drafted,
    /// 活跃——agent 当前持有这个判断；可被引用开仓
    Active,
    /// 已兑现——validation_checks 大半触发且行情验证，归档
    Validated,
    /// 漂移——不再适用但未被反向证伪（e.g. 节奏判断错了，但方向没错）
    Drifted,
    /// 已证伪——invalidation 条件触发，必须撤
    Invalidated,
    /// 主动放弃——agent / 用户决定不再跟踪（与证伪不同：未被市场验证）
    Abandoned,
}

impl ThesisState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Drafted => "drafted",
            Self::Active => "active",
            Self::Validated => "validated",
            Self::Drifted => "drifted",
            Self::Invalidated => "invalidated",
            Self::Abandoned => "abandoned",
        }
    }

    /// 终态——不再变化。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Validated | Self::Drifted | Self::Invalidated | Self::Abandoned
        )
    }

    pub fn is_open(self) -> bool {
        matches!(self, Self::Drafted | Self::Active)
    }
}

// ====== Event 链 ========================================================

/// Thesis 事件链——状态机转换 + 用户反馈，append-only。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ThesisEvent {
    Created,
    Activated,
    /// validation_checks 第 N 条命中。
    ValidationCheckHit { check_index: usize, note: Option<String> },
    /// invalidation 条件触发（结构化条件未实现前，传整段文本）。
    InvalidationCheckHit { note: String },
    Drifted { reason: String },
    Invalidated { reason: String },
    Validated { reason: String },
    Abandoned { reason: String },
    /// 用户对此 thesis 的反馈（reflection 时一起参考，但不立即变 principle）。
    UserFeedback { text: String },
}

impl ThesisEvent {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Activated => "activated",
            Self::ValidationCheckHit { .. } => "validation_check_hit",
            Self::InvalidationCheckHit { .. } => "invalidation_check_hit",
            Self::Drifted { .. } => "drifted",
            Self::Invalidated { .. } => "invalidated",
            Self::Validated { .. } => "validated",
            Self::Abandoned { .. } => "abandoned",
            Self::UserFeedback { .. } => "user_feedback",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThesisEventRecord {
    pub thesis_id: ThesisId,
    pub event: ThesisEvent,
    pub occurred_at: OccurredAt,
}

// ====== 构造 / 转换辅助 =================================================

impl Thesis {
    /// 新建 draft thesis（agent create_thesis 工具入口）。
    pub fn draft(
        hypothesis: String,
        invalidation: String,
        validation_checks: Vec<String>,
        conviction: Conviction,
        target_codes: Vec<StockCode>,
        regime: Option<Regime>,
        now: OccurredAt,
    ) -> Self {
        Self {
            id: ThesisId::new(),
            hypothesis,
            invalidation,
            validation_checks,
            conviction,
            state: ThesisState::Drafted,
            target_codes,
            regime_at_creation: regime,
            created_at: now,
            updated_at: now,
            closed_at: None,
        }
    }

    /// 一次创建即激活（open_position 内联 new_thesis 时用）。
    pub fn active(
        hypothesis: String,
        invalidation: String,
        validation_checks: Vec<String>,
        conviction: Conviction,
        target_codes: Vec<StockCode>,
        regime: Option<Regime>,
        now: OccurredAt,
    ) -> Self {
        let mut t = Self::draft(
            hypothesis,
            invalidation,
            validation_checks,
            conviction,
            target_codes,
            regime,
            now,
        );
        t.state = ThesisState::Active;
        t
    }
}
