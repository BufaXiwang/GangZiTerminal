//! AgentRun / AgentRunProfile —— spec `agent-runtime-module.md §2`。
//!
//! `AgentRunPriority` 与 `select_profile` 是 spec §7 / §9 的 canonical API surface，
//! 在 scheduled_review / manual_replay run pipeline 接入前保留 enum 完整性。

#![allow(dead_code)] // AgentRunPriority / select_profile 是 spec §7/§9 canonical 接口

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunProfileId {
    UserChat,
    NewsAnalysis,
    AccountTriggerResponse,
    ScheduledReview,
    ManualReplay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentRunPriority {
    P0,
    P1,
    P2,
    P3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentRunTrigger {
    UserChat { message_id: String },
    NewsBatch { news_ids: Vec<String> },
    AccountTrigger { trigger_id: String },
    ScheduledReview { reason: String },
    ManualReplay { ref_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRun {
    pub run_id: String,
    pub profile_id: AgentRunProfileId,
    pub episode_ids: Vec<String>,
    pub trigger: AgentRunTrigger,
    pub provider: String,
    pub wire_format: String,
    pub model: String,
    pub status: AgentRunStatus,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub error: Option<String>,
}

/// 选择 profile（spec §2 默认表）。
pub fn select_profile(trigger: &AgentRunTrigger) -> AgentRunProfileId {
    match trigger {
        AgentRunTrigger::UserChat { .. } => AgentRunProfileId::UserChat,
        AgentRunTrigger::NewsBatch { .. } => AgentRunProfileId::NewsAnalysis,
        AgentRunTrigger::AccountTrigger { .. } => AgentRunProfileId::AccountTriggerResponse,
        AgentRunTrigger::ScheduledReview { .. } => AgentRunProfileId::ScheduledReview,
        AgentRunTrigger::ManualReplay { .. } => AgentRunProfileId::ManualReplay,
    }
}
