//! Agent 推理输入/输出的类型定义。
//!
//! 命名空间约定：
//! - `*Result` 是 Agent 解析出来的结构化输出（briefing / review / chat reply）
//! - `*Update` 是 Agent 主动写出的增量
//! - 其他类型是构造 prompt 时需要从 SQLite 读出的"上下文"
//!
//! 所有字段使用 camelCase 序列化，匹配 Agent 输出和现有前端类型。

use serde::{Deserialize, Serialize};

// ====== 投资者长期记忆 ======

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InvestorMemory {
    pub focus_themes: Vec<String>,
    pub preferred_markets: Vec<String>,
    pub risk_preference: String,
    pub learning_goals: Vec<String>,
    pub known_biases: Vec<String>,
    pub investment_principles: Vec<String>,
    pub watch_questions: Vec<String>,
    pub recent_insights: Vec<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InvestorMemoryUpdate {
    pub focus_themes: Option<Vec<String>>,
    pub preferred_markets: Option<Vec<String>>,
    pub risk_preference: Option<String>,
    pub learning_goals: Option<Vec<String>>,
    pub known_biases: Option<Vec<String>>,
    pub investment_principles: Option<Vec<String>>,
    pub watch_questions: Option<Vec<String>>,
    pub recent_insights: Option<Vec<String>>,
}

// ====== 学习画像 ======

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LearningProfile {
    pub level: i32,
    pub score: i32,
    pub total_records: i32,
    pub reviewed_records: i32,
    pub review_rate: f64,
    pub validated_count: i32,
    pub invalidated_count: i32,
    pub watching_count: i32,
    pub inconclusive_count: i32,
    pub top_themes: Vec<NameCount>,
    pub common_mistakes: Vec<TextCount>,
    pub focus_suggestions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NameCount {
    pub name: String,
    pub count: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextCount {
    pub text: String,
    pub count: i32,
}

// ====== Briefing 输出 ======

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BriefingResult {
    pub headline: String,
    pub summary_md: String,
    pub signals: Vec<BriefingSignal>,
    pub trade_calls: Vec<BriefingTradeCall>,
    pub covered_news_ids: Vec<String>,
    pub next_focus: Vec<String>,
    pub highlight: Option<BriefingHighlight>,
    pub memory_updates: InvestorMemoryUpdate,
    pub memory_removals: InvestorMemoryUpdate,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BriefingSignal {
    pub theme: String,
    pub direction: String,
    pub evidence: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BriefingTradeCall {
    pub code: Option<String>,
    pub name: String,
    pub action: String,
    pub thesis: String,
    pub trigger_condition: String,
    pub invalidation_condition: String,
    pub stop_loss: Option<String>,
    pub take_profit: Option<String>,
    pub time_stop: Option<String>,
    pub risk_level: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BriefingHighlight {
    pub importance: String,
    pub message: String,
}

// ====== Review 输出 ======

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReviewResult {
    pub summary: String,
    pub thesis_status: String,
    pub confidence: f64,
    pub evidence: Vec<String>,
    pub price_action: Vec<String>,
    pub news_follow_up: Vec<String>,
    pub checklist_review: Vec<String>,
    pub mistakes: Vec<String>,
    pub next_actions: Vec<String>,
    pub learning_update: String,
    pub reviewed_at: String,
    pub next_review_at: Option<String>,
}

// ====== Chat reply 输出 ======

// ChatReplyResult 已删除——chat 不再走 "agent 输出 JSON 我 parse" 协议；
// pipeline::chat 直接拿 agent loop 的最终文本写消息，memory 由 agent 通过 update_memory
// 工具自己写入。pipeline/chat.rs 里有同名结构体但只是 Tauri 命令的返回值，与此无关。

// ====== Stored entities（从 SQLite 读出的，用于 prompt 上下文） ======

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredNewsItem {
    pub id: String,
    pub title: String,
    pub source: String,
    pub published: Option<String>,
    pub summary: Option<String>,
    pub link: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimulatedPosition {
    pub id: String,
    pub code: String,
    pub name: String,
    /// 首次开仓价（归档不变）——加仓后用 `avg_entry_price` 而不是这个算盈亏。
    pub entry_price: f64,
    /// 首次开仓股数（归档不变）——老数据迁移时也是 `original_shares` 的值。
    /// 加仓后**不**更新这个字段；展示当前持仓股数请用 `current_shares()`。
    pub shares: i64,
    pub entry_at: String,
    pub exit_price: Option<f64>,
    pub exit_at: Option<String>,
    pub close_reason: Option<String>,
    pub thesis: String,
    pub stop_loss: Option<f64>,
    pub take_profit: Option<f64>,
    /// 时间止损绝对时间（ISO 8601）。开仓时根据 prompt 给的 timeStop 文案算出，
    /// 简化处理：当前用 entry_at + 7 天日历日（覆盖 A 股大约 5 个交易日的 timeStop 提示）。
    /// scheduler 在 quote refresh 时检测 now >= time_stop_at → 触发时间止损平仓。
    /// 旧仓位（重构前开的）反序列化得 None，detect 函数自动跳过。
    #[serde(default)]
    pub time_stop_at: Option<String>,
    pub source_analysis_id: String,
    pub status: String, // "open" | "closed"
    /// 首次开仓股数。新仓位写入；老数据反序列化时为 None，访问走 `original_shares()`
    /// fallback 到 `shares`。
    #[serde(default)]
    pub original_shares: Option<i64>,
    /// 当前剩余股数。加减仓后更新；老数据为 None，访问走 `current_shares()` fallback 到 `shares`。
    /// 平仓后变 0（status=closed）。
    #[serde(default)]
    pub current_shares: Option<i64>,
    /// 加权均价。加仓时按 `(旧均价×旧股 + 新价×加股)/(旧股+加股)` 更新；
    /// 老数据为 None，访问走 `avg_entry_price()` fallback 到 `entry_price`。
    /// 盈亏计算用这个，不要用 `entry_price`。
    #[serde(default)]
    pub avg_entry_price: Option<f64>,
}

impl SimulatedPosition {
    /// 当前剩余股数——新仓位走 `current_shares` 字段，老数据回退到 `shares`。
    pub fn current_shares(&self) -> i64 {
        self.current_shares.unwrap_or(self.shares)
    }

    /// 首次开仓股数——审计基线，永不变。复盘场景从 events 算盈亏归因时用。
    #[allow(dead_code)]
    pub fn original_shares(&self) -> i64 {
        self.original_shares.unwrap_or(self.shares)
    }

    /// 加权均价——盈亏计算用这个；老数据回退到首次开仓价。
    pub fn avg_entry_price(&self) -> f64 {
        self.avg_entry_price.unwrap_or(self.entry_price)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredAnalysisRecord {
    pub id: String,
    pub item: StoredNewsItem,
    pub result: AnalysisResult,
    pub created_at: String,
    pub next_review_at: Option<String>,
    pub review: Option<ReviewResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisResult {
    pub summary: String,
    pub related_stocks: Vec<String>,
    pub key_facts: Vec<String>,
    pub sectors: Vec<String>,
    pub themes: Vec<String>,
    pub impact: String,
    pub confidence: f64,
    pub time_horizon: String,
    pub reasoning: Vec<String>,
    pub risks: Vec<String>,
    pub verification_checklist: Vec<String>,
    pub external_research: ExternalResearch,
    pub learning_notes: String,
    pub decision: String,
    pub trade_plan: SimulatedTradePlan,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExternalResearch {
    pub used: bool,
    pub queries: Vec<String>,
    pub findings: Vec<String>,
    pub sources: Vec<ExternalSource>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExternalSource {
    pub title: String,
    pub url: Option<String>,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimulatedTradePlan {
    pub action: String,
    pub suitability: String,
    pub target_stocks: Vec<TargetStock>,
    pub entry_strategy: EntryStrategy,
    pub position_sizing: PositionSizing,
    pub exit_plan: ExitPlan,
    pub risk_level: String,
    pub confidence: f64,
    pub why_not_buy_now: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TargetStock {
    pub name: String,
    pub code: Option<String>,
    pub reason: String,
    pub priority: i32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryStrategy {
    pub style: String,
    pub trigger_condition: String,
    pub invalidation_condition: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionSizing {
    pub suggested_weight: String,
    pub max_loss_per_trade: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitPlan {
    pub take_profit_condition: String,
    pub stop_loss_condition: String,
    pub time_stop: String,
}

// ====== 持仓事件 ======

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionEvent {
    pub id: String,
    pub position_id: String,
    pub event_kind: String,
    pub occurred_at: String,
    pub source_kind: Option<String>,
    pub source_ref: Option<String>,
    pub payload: Option<serde_json::Value>,
    pub agent_note_md: Option<String>,
}
