//! ContextBundle / 压缩策略类型。
//!
//! Spec: docs/design/agent-infra-module.md §2 `ContextBundle`，§4 上下文管理 + 压缩策略

use crate::domain::shared::Freshness;
use serde::{Deserialize, Serialize};
use specta::Type;

use super::messages::JsonSummary;

/// Context part 类型（spec §2 / §4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ContextPartKind {
    System,
    Realtime,
    Chat,
    Memory,
    /// Spec §2: 易腐 skill 结果被压缩成 stub 时，content 文本是
    /// `<skill_result_stub name="..." call_id="..." ref="..." />`。
    SkillResultStub,
}

/// Context content 多形态承载。
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
#[serde(untagged)]
pub enum ContextContent {
    Text(String),
    Json(JsonSummary),
}

/// 单个 context part。
///
/// Spec: agent-infra-module.md §2 `ContextPart`
//
// 注：`freshness: Option<Freshness>` 中 `Freshness` 不 impl PartialEq，因此本结构不 derive PartialEq。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ContextPart {
    pub kind: ContextPartKind,
    pub content: ContextContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness: Option<Freshness>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_estimate: Option<u32>,
    pub droppable: bool,
}

impl ContextPart {
    /// 估算 token 数：优先使用 caller 提供的 estimate，否则按 4 字符 / token 启发式。
    pub fn estimated_tokens(&self) -> u32 {
        if let Some(t) = self.token_estimate {
            return t;
        }
        let len = match &self.content {
            ContextContent::Text(s) => s.chars().count(),
            ContextContent::Json(v) => v.to_string().chars().count(),
        };
        ((len + 3) / 4) as u32
    }
}

/// Runtime 提交给 Infra 的上下文包。
///
/// Spec: agent-infra-module.md §2 `ContextBundle`，§4 四类上下文分层
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ContextBundle {
    pub run_id: String,
    pub system_parts: Vec<ContextPart>,
    pub realtime_parts: Vec<ContextPart>,
    pub chat_parts: Vec<ContextPart>,
    pub memory_parts: Vec<ContextPart>,
}

impl ContextBundle {
    pub fn new(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            system_parts: vec![],
            realtime_parts: vec![],
            chat_parts: vec![],
            memory_parts: vec![],
        }
    }

    /// 估算 bundle 总 token 数。
    pub fn estimated_tokens(&self) -> u32 {
        let sum = self
            .system_parts
            .iter()
            .chain(self.realtime_parts.iter())
            .chain(self.chat_parts.iter())
            .chain(self.memory_parts.iter())
            .map(ContextPart::estimated_tokens)
            .map(u64::from)
            .sum::<u64>();
        sum.min(u32::MAX as u64) as u32
    }
}

/// Compact 触发阶段（spec §4 表格）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum CompactTier {
    MicroClear,
    Summarize,
    Drop,
    ReactiveRetry,
}

/// Context 窗口限制策略（spec §4 soft / summarize / hard）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ContextWindowLimits {
    pub soft_limit_tokens: u32,
    pub summarize_threshold_tokens: u32,
    pub hard_limit_tokens: u32,
    /// time-based MicroClear 间隔；默认 60 分钟（spec §4 表格）。
    pub micro_clear_after_secs: u64,
}

impl Default for ContextWindowLimits {
    /// 保守默认值；具体生产值应由 Runtime / channel config 注入。
    fn default() -> Self {
        Self {
            soft_limit_tokens: 60_000,
            summarize_threshold_tokens: 90_000,
            hard_limit_tokens: 180_000,
            micro_clear_after_secs: 60 * 60,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(kind: ContextPartKind, text: &str, droppable: bool) -> ContextPart {
        ContextPart {
            kind,
            content: ContextContent::Text(text.into()),
            freshness: None,
            token_estimate: None,
            droppable,
        }
    }

    #[test]
    fn context_bundle_estimates_tokens_across_lanes() {
        let mut b = ContextBundle::new("r1");
        b.system_parts
            .push(part(ContextPartKind::System, &"a".repeat(40), false));
        b.chat_parts
            .push(part(ContextPartKind::Chat, &"b".repeat(80), true));
        // 40 / 4 + 80 / 4 = 10 + 20 = 30
        assert_eq!(b.estimated_tokens(), 30);
    }

    #[test]
    fn context_part_kind_serde_snake() {
        let p = part(ContextPartKind::SkillResultStub, "x", true);
        let j = serde_json::to_value(&p).unwrap();
        assert_eq!(j["kind"], "skill_result_stub");
        assert_eq!(j["droppable"], true);
    }

    #[test]
    fn context_window_limits_default_reasonable() {
        let d = ContextWindowLimits::default();
        assert!(d.soft_limit_tokens < d.hard_limit_tokens);
        assert!(d.summarize_threshold_tokens <= d.hard_limit_tokens);
    }
}
