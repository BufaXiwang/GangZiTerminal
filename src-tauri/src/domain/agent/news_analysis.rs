//! Agent 对 news 的分析状态机——一等概念（News BC 不感知此类型）。
//!
//! 状态迁移：
//! ```
//!   pending     ──claim_batch(M)──►   processing
//!   processing  ──mark_consumed────►  consumed     (agent run 成功)
//!   processing  ──revert──────────►   pending      (agent run 失败 → 等下次重试)
//!   processing  ──watchdog 30min──►   pending      (孤儿回收)
//!   processing  ──mark_failed─────►   failed       (预留：agent 显式拒绝)
//! ```

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewsAnalysisStatus {
    Pending,
    Processing,
    Consumed,
    Failed,
}

impl NewsAnalysisStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::Consumed => "consumed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "processing" => Some(Self::Processing),
            "consumed" => Some(Self::Consumed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}
