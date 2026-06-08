//! Agent Runtime domain — 业务运行期 canonical 类型。
//!
//! Spec: docs/design/agent-runtime-module.md §3 领域模型
//!
//! 精简概念集（3 + AgentRun + 报告文件）：
//! - [`AgentRun`]            — 一次 Agent 运行；决策链主键（`run_id`）
//! - [`InvestmentStrategy`]  — 投资策略（一段自然语言）；版本化；只在对话中用户确认后写
//! - [`AnalysisResult`]      — news 分析产物（action / no_action）；前端右侧列表
//! - [`AgentTrade`]          — 每次 `operate_account` 的审计戳（runtime 侧，不耦合 Account）
//! - 复盘报告               — review sub-agent 产物，workspace 文件，不入库
//!
//! 证据不单独持久化：靠 `run_id → ToolCall 审计` 重建（spec §3「证据」）。

use crate::domain::shared::{ErrorCode, OccurredAt, TradeDate, TsCode};
use serde::{Deserialize, Serialize};
use specta::Type;

use super::channel::WireFormat;

// ----------------------------------------------------------------------------
// AgentRun
// ----------------------------------------------------------------------------

/// 四种运行模式（spec §3 mode 表）。trigger 与 mode 一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunMode {
    /// 用户消息驱动的连续对话线程。
    Dialogue,
    /// news buffer M/N 触发的隔离单次分析。
    News,
    /// `account-triggered` 实时唤起（止损/成交等）。
    AccountTrigger,
    /// 收盘调度 / 被对话·news fork 的只读复盘（永不下单）。
    Review,
}

/// run 的触发来源（spec §3 `AgentRunTrigger`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum AgentRunTrigger {
    UserChat { message_id: String },
    NewsBatch { news_ids: Vec<String> },
    AccountTrigger { trigger_id: String },
    EodReview { trade_date: TradeDate },
}

impl AgentRunTrigger {
    /// 该 trigger 对应的 mode。
    pub fn mode(&self) -> AgentRunMode {
        match self {
            AgentRunTrigger::UserChat { .. } => AgentRunMode::Dialogue,
            AgentRunTrigger::NewsBatch { .. } => AgentRunMode::News,
            AgentRunTrigger::AccountTrigger { .. } => AgentRunMode::AccountTrigger,
            AgentRunTrigger::EodReview { .. } => AgentRunMode::Review,
        }
    }
}

/// run 生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// 一次 Agent run —— 决策链主键。
///
/// Spec: agent-runtime-module.md §3 `AgentRun`
///
/// 规则：
/// - `run_id` 是事件 / 工具调用 / 模型 turn / AnalysisResult / AgentTrade 的关联键。
/// - **策略版本冻结**：run 创建时把 active `version` 写入 `strategy_version`，全程不变。
/// - `review` run 可由收盘调度起（顶层，`trigger=eod_review`），也可被对话/news fork
///   （`parent_run_id` 指向父 run）；两种都只读、不下单。
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentRun {
    pub run_id: String,
    pub mode: AgentRunMode,
    pub trigger: AgentRunTrigger,
    /// review 被对话/news fork 时指向父 run。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    pub provider: String,
    pub wire_format: WireFormat,
    pub model: String,
    /// run 创建时冻结的 active 策略版本（无策略时缺省）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy_version: Option<u32>,
    /// account_trigger run 经 `orderId → runId` 反查关联到原始建仓 run。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_run_id: Option<String>,
    pub status: AgentRunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<OccurredAt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<OccurredAt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ----------------------------------------------------------------------------
// InvestmentStrategy
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum StrategyStatus {
    Active,
    Paused,
}

/// 投资策略（一段自然语言）——版本化，只在对话中用户确认后写。
///
/// Spec: agent-runtime-module.md §3 `InvestmentStrategy`
///
/// 本阶段纯自然语言（不结构化硬约束）；硬风控由 Account fail-closed + Runtime 编排兜底。
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InvestmentStrategy {
    pub strategy_id: String,
    pub version: u32,
    /// 自然语言：投资理念 / 选股 / 风控 / 仓位 / 止盈止损纪律。
    pub strategy: String,
    pub status: StrategyStatus,
    pub created_at: OccurredAt,
    pub updated_at: OccurredAt,
}

// ----------------------------------------------------------------------------
// AnalysisResult
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisResultKind {
    /// 触发了动作（下单 / 改自选）。
    Action,
    /// 明确不行动（大多数 news 应是此）。
    NoAction,
}

/// news 分析产物 —— 前端右侧列表。
///
/// Spec: agent-runtime-module.md §3 `AnalysisResult`
///
/// `no_action` 也要 emit；证据按 `run_id` 拉 `fetch_news` ToolCall，不另存快照。
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisResult {
    pub result_id: String,
    pub run_id: String,
    pub kind: AnalysisResultKind,
    /// 结论 + 理由（含「为什么现在进还来得及 / 已 price-in」判断）。
    pub summary: String,
    pub related_codes: Vec<TsCode>,
    /// 若 kind=action 且下单，关联 AgentTrade。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trade_ids: Vec<String>,
    pub created_at: OccurredAt,
}

// ----------------------------------------------------------------------------
// AgentTrade
// ----------------------------------------------------------------------------

/// AgentTrade 两态（spec §3）：`submitting`=调用前已落库；`settled`=拿到 Account 结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum AgentTradeStatus {
    Submitting,
    Settled,
}

/// 指向 Account 返回的稳定 ID（spec §3 `AccountResultRef`）。
///
/// Account 不知 `tradeId/runId/strategyVersion`；只回这些自有 ID。
#[derive(Debug, Clone, Default, Serialize, Deserialize, Type, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AccountResultRef {
    pub accepted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fill_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub account_event_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<ErrorCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// 每次 `operate_account` 的审计戳 —— runtime 侧、不耦合 Account。
///
/// Spec: agent-runtime-module.md §3 `AgentTrade`
///
/// 崩溃恢复：调 Account 前先落 `submitting`，返回后填 `account_result_ref` 转 `settled`；
/// 启动恢复用 `client_order_id` 反查对账（Account 侧去重为延后加固项）。
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentTrade {
    pub trade_id: String,
    pub run_id: String,
    /// 幂等键：下单前生成，传给 Account 去重 + 恢复对账。
    pub client_order_id: String,
    /// 来自 `AgentRun.strategy_version`（冻结值）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy_version: Option<u32>,
    pub reason: String,
    /// `OperateAccountInput` 的可读摘要（不重定义账户命令）。
    pub account_input_summary: String,
    pub status: AgentTradeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_result_ref: Option<AccountResultRef>,
    pub created_at: OccurredAt,
    pub updated_at: OccurredAt,
}

// ----------------------------------------------------------------------------
// ReviewSuggestion
// ----------------------------------------------------------------------------

/// 一条复盘策略建议 —— review run 经 `record_review_suggestion` 工具声明、Runtime 持久化。
///
/// Spec: agent-runtime-module.md §3 复盘报告 ④ 上次建议 follow-up
///
/// 下次 review 读上一交易日的 `ReviewSuggestion` + 策略版本历史 → 确定性对账「该建议后是否发生过
/// `upsert_investment_strategy` 采纳」（按时间），在 follow-up 段确定性写出。**不自动改策略**——
/// 建议只是文本，采纳与否由用户在对话中确认。
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReviewSuggestion {
    pub suggestion_id: String,
    /// 产出该建议的 review run。
    pub review_run_id: String,
    /// 该建议所属交易日（CN）。
    pub trade_date: TradeDate,
    /// 策略建议文本。
    pub text: String,
    pub created_at: OccurredAt,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(v: &T) {
        let j = serde_json::to_string(v).unwrap();
        let back: T = serde_json::from_str(&j).unwrap();
        assert_eq!(&back, v);
    }

    #[test]
    fn trigger_mode_mapping() {
        assert_eq!(
            AgentRunTrigger::UserChat { message_id: "m".into() }.mode(),
            AgentRunMode::Dialogue
        );
        assert_eq!(
            AgentRunTrigger::NewsBatch { news_ids: vec!["n1".into()] }.mode(),
            AgentRunMode::News
        );
        assert_eq!(
            AgentRunTrigger::AccountTrigger { trigger_id: "t".into() }.mode(),
            AgentRunMode::AccountTrigger
        );
        assert_eq!(
            AgentRunTrigger::EodReview { trade_date: TradeDate::parse("20260603").unwrap() }.mode(),
            AgentRunMode::Review
        );
    }

    #[test]
    fn agent_run_serde_roundtrip() {
        rt(&AgentRun {
            run_id: "r1".into(),
            mode: AgentRunMode::News,
            trigger: AgentRunTrigger::NewsBatch { news_ids: vec!["n1".into(), "n2".into()] },
            parent_run_id: None,
            provider: "anthropic".into(),
            wire_format: WireFormat::Messages,
            model: "claude".into(),
            strategy_version: Some(3),
            causation_run_id: None,
            status: AgentRunStatus::Running,
            started_at: None,
            ended_at: None,
            error: None,
        });
    }

    #[test]
    fn analysis_result_trigger_kind_serde() {
        // action/no_action 都能 roundtrip；trigger 标签 camelCase。
        let j = serde_json::to_string(&AnalysisResultKind::NoAction).unwrap();
        assert_eq!(j, "\"no_action\"");
        let t = AgentRunTrigger::AccountTrigger { trigger_id: "tg1".into() };
        let jt = serde_json::to_string(&t).unwrap();
        assert!(jt.contains("\"kind\":\"account_trigger\""), "{jt}");
        assert!(jt.contains("\"triggerId\":\"tg1\""), "{jt}");
    }

    #[test]
    fn agent_trade_submitting_then_settled() {
        let mut tr = AgentTrade {
            trade_id: "td1".into(),
            run_id: "r1".into(),
            client_order_id: "co1".into(),
            strategy_version: Some(3),
            reason: "news 利好开仓".into(),
            account_input_summary: "buy 600519 100".into(),
            status: AgentTradeStatus::Submitting,
            account_result_ref: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        rt(&tr);
        tr.status = AgentTradeStatus::Settled;
        tr.account_result_ref = Some(AccountResultRef {
            accepted: true,
            order_id: Some("ord1".into()),
            fill_ids: vec!["f1".into()],
            position_id: Some("pos1".into()),
            account_event_ids: vec!["e1".into()],
            rejection_event_id: None,
            reason: None,
            message: None,
        });
        rt(&tr);
    }

    #[test]
    fn investment_strategy_roundtrip() {
        rt(&InvestmentStrategy {
            strategy_id: "s1".into(),
            version: 2,
            strategy: "价值优先，单票不超 25%，跌破成本 8% 止损，不确定不交易。".into(),
            status: StrategyStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        });
    }
}
