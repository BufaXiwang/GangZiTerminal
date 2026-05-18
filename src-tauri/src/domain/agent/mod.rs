#![allow(dead_code, unused_imports)] // canonical 类型完整面：部分字段/variants 按 wire format 需要保留

//! Domain `agent`——Agent 决策子域。
//!
//! v3 设计（见 docs/design/agent-v3-expectation-driven.md）核心实体：
//! - `signal`：SignalKind 24 枚举 + NewsKind / NewsImportance / EventKind / SignalDetection
//! - `strategy`：Strategy DSL（trigger_when + target_rule + track record）
//! - `lesson`：每个 expectation 终态自动生成的原子观察
//! - `types`：Block / Message / AgentEvent / AgentRequest 等 wire canonical 形态
//!
//! v2 残留（W22 schema 升级到 v3 时整体下线）：
//! - `principle`：被 heuristic 取代——heuristic 在 W22 落 infra 时迁移
//!
//! `ChatProvider` trait 在 `infrastructure::agent::provider`，`Tool` trait 在
//! `adapters::agent_tools`——两者都是协议适配，不属于 domain。
//! identity.md（Agent 人设档案）在 `pipeline::agent`，由 `prompt.rs` `include_str!` 读入。

pub mod heuristic;
pub mod lesson;
pub mod principle; // v2 残留，W22 下线
pub mod strategy;
pub mod types;

pub use heuristic::{
    EffectiveState, Heuristic, HeuristicCategory, HeuristicId, HeuristicOrigin,
    HEURISTIC_BODY_MAX_CHARS,
};
pub use lesson::{Lesson, LessonId, LessonOutcome};
pub use principle::{
    Principle, PrincipleCategory, PrincipleId, PrincipleOrigin, PrincipleState,
    PRINCIPLE_BODY_MAX_CHARS,
};
// SignalKind / NewsKind / NewsImportance / EventKind 等迁到 domain/shared::signal
// （三个 BC 都引用——shared vocabulary）。从这里 re-export 让旧 use 路径仍可工作。
pub use crate::domain::shared::{
    EventKind, NewsImportance, NewsKind, SignalDetection, SignalKind,
};
pub use strategy::{
    ConvictionRule, SignalCondition, Strategy, StrategyEvent, StrategyEventRecord, StrategyId,
    TargetRule, TriggerLogic,
};
pub use types::ProviderKind;
