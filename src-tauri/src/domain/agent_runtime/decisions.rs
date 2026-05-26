//! DecisionEpisode / EvidenceRef / TradeIntent / DecisionReview / StrategyCard
//! 类型骨架——对齐 spec `agent-runtime-module.md §2`。
//!
//! 第一阶段只声明类型；完整持久化 / hydrate / state machine 后续 phase 落地。
//!
//! `Evidence*Snapshot` typed 结构是 spec §2 长期证据 schema 的 Rust contract；
//! hydrate 路径目前用 `serde_json::Value` 直通，typed 结构提供字段名锁定 +
//! round-trip 测试，B6 P2 集成进 adapter hydrate 层。

#![allow(dead_code)] // typed snapshot contract 类型；adapter 集成在 B6

use serde::{Deserialize, Serialize};

// ============ DecisionEpisode ===========================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeAction {
    NoAction,
    AddWatchlist,
    RemoveWatchlist,
    PlaceOrder,
    CancelOrder,
    OpenPosition,
    ScalePosition,
    ClosePosition,
    AdjustProtection,
    RecordInvalidationSignal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeActionStatus {
    NoAction,
    Intended,
    Submitted,
    Blocked,
    Deferred,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskPlan {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_position_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_loss: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub take_profit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalidation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionEpisode {
    pub episode_id: String,
    pub run_id: String,
    pub trigger_kind: String,
    pub symbols: Vec<String>,
    pub thesis: String,
    pub action: EpisodeAction,
    pub action_status: EpisodeActionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk_plan: Option<RiskPlan>,
    pub strategy_ids: Vec<String>,
    pub evidence_refs: Vec<EvidenceRef>,
    pub created_at: String,
}

// ============ EvidenceRef ===============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvidenceRef {
    News {
        id: String,
        snapshot: serde_json::Value,
    },
    Quote {
        id: String,
        snapshot: serde_json::Value,
    },
    AccountSnapshot {
        id: String,
        snapshot: serde_json::Value,
    },
    Position {
        id: String,
        snapshot: serde_json::Value,
    },
    Order {
        id: String,
        snapshot: serde_json::Value,
    },
    AccountTrigger {
        id: String,
        snapshot: serde_json::Value,
    },
    Strategy {
        id: String,
        snapshot: serde_json::Value,
    },
    ToolCall {
        id: String,
        snapshot: serde_json::Value,
    },
}

// ============ Typed Evidence Snapshots ==================================
//
// spec `agent-runtime-module.md §2` 定义 `Evidence*Snapshot` 是 episode/review
// 的长期证据 schema。本节给出 Rust 类型契约，hydrate 时序列化的 JSON 字段名
// 与 spec 对齐（camelCase）；运行时 `EvidenceRef.snapshot` 仍是 `Value` 透传
// 以便宽松解码，但所有写入路径建议先经 typed struct 验证。

/// spec §2 `EvidenceSnapshotBase` —— 全部 snapshot 共享的元字段。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceSnapshotBase {
    pub schema_version: u32,
    pub captured_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<crate::domain::shared::Freshness>,
}

impl EvidenceSnapshotBase {
    pub fn now(source: Option<String>) -> Self {
        Self {
            schema_version: 1,
            captured_at: chrono::Utc::now().to_rfc3339(),
            source,
            freshness: None,
        }
    }
}

/// spec §2 `EvidenceNewsSnapshot`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceNewsSnapshot {
    #[serde(flatten)]
    pub base: EvidenceSnapshotBase,
    pub news_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub article_excerpt: Option<String>,
}

/// spec §2 `EvidenceQuoteSnapshot`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceQuoteSnapshot {
    #[serde(flatten)]
    pub base: EvidenceSnapshotBase,
    pub ts_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pe_ttm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pb: Option<f64>,
}

/// spec §2 `EvidenceAccountSnapshot`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceAccountSnapshot {
    #[serde(flatten)]
    pub base: EvidenceSnapshotBase,
    pub cash: f64,
    pub total_assets: f64,
    pub market_value: f64,
    pub total_pnl: f64,
    pub open_position_count: usize,
    pub pending_order_count: usize,
}

/// spec §2 `EvidencePositionSnapshot`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidencePositionSnapshot {
    #[serde(flatten)]
    pub base: EvidenceSnapshotBase,
    pub position_id: String,
    pub ts_code: String,
    pub quantity: i64,
    pub sellable_quantity: i64,
    pub avg_cost: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unrealized_pnl: Option<f64>,
}

/// spec §2 `EvidenceOrderSnapshot`。`side` / `order_type` / `status` 全部
/// 走 canonical enum，不接受任意字符串。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceOrderSnapshot {
    #[serde(flatten)]
    pub base: EvidenceSnapshotBase,
    pub order_id: String,
    pub ts_code: String,
    pub side: crate::domain::account::OrderSide,
    pub order_type: crate::domain::account::OrderType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_price: Option<f64>,
    pub quantity: i64,
    pub filled_quantity: i64,
    pub status: crate::domain::account::OrderStatus,
}

/// spec §2 `EvidenceAccountTriggerSnapshot`。`trigger_type` 走 canonical enum。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceAccountTriggerSnapshot {
    #[serde(flatten)]
    pub base: EvidenceSnapshotBase,
    pub trigger_id: String,
    pub trigger_type: crate::domain::shared::AccountTriggerKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    pub occurred_at: String,
}

/// spec §2 `EvidenceStrategySnapshot`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceStrategySnapshot {
    #[serde(flatten)]
    pub base: EvidenceSnapshotBase,
    pub strategy_id: String,
    pub version: u32,
    pub name: String,
    pub description: String,
    pub status: String,
    pub config_summary: String,
}

/// spec §2 `ToolCallEvidenceSnapshot`。注意 spec 上没有 EvidenceSnapshotBase
/// flatten —— ToolCall snapshot 是独立 schema。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallEvidenceSnapshot {
    pub schema_version: u32,
    pub name: String,
    pub input_summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_summary: Option<String>,
    pub is_error: bool,
    pub captured_at: String,
}

#[cfg(test)]
mod typed_snapshot_tests {
    //! spec `agent-runtime-module.md §2` 字段名锁定：JSON 序列化的 key 必须严格
    //! 匹配 spec 定义（camelCase）。这些 round-trip 测试一旦失败说明 JSON
    //! contract 漂移，消费方（前端 / Runtime）会读不到字段。

    use super::*;
    use serde_json::Value;

    fn base() -> EvidenceSnapshotBase {
        EvidenceSnapshotBase {
            schema_version: 1,
            captured_at: "2026-01-01T00:00:00Z".into(),
            source: Some("test".into()),
            freshness: None,
        }
    }

    #[test]
    fn news_snapshot_camel_case_keys() {
        let s = EvidenceNewsSnapshot {
            base: base(),
            news_id: "n1".into(),
            title: "t".into(),
            summary: None,
            url: None,
            published_at: Some("2026-01-01".into()),
            article_excerpt: None,
        };
        let v: Value = serde_json::to_value(&s).unwrap();
        assert!(v.get("schemaVersion").is_some());
        assert!(v.get("capturedAt").is_some());
        assert!(v.get("newsId").is_some());
        assert!(v.get("publishedAt").is_some());
    }

    #[test]
    fn quote_snapshot_camel_case_keys() {
        let s = EvidenceQuoteSnapshot {
            base: base(),
            ts_code: "600519.SH".into(),
            name: None,
            price: Some(1700.0),
            change_percent: Some(1.5),
            volume: None,
            amount: None,
            pe_ttm: Some(30.0),
            pb: None,
        };
        let v: Value = serde_json::to_value(&s).unwrap();
        assert!(v.get("tsCode").is_some());
        assert!(v.get("changePercent").is_some());
        assert!(v.get("peTtm").is_some());
    }

    #[test]
    fn account_trigger_snapshot_camel_case_keys() {
        let s = EvidenceAccountTriggerSnapshot {
            base: base(),
            trigger_id: "tr1".into(),
            trigger_type: crate::domain::shared::AccountTriggerKind::StopLoss,
            ts_code: None,
            position_id: Some("p1".into()),
            order_id: None,
            occurred_at: "2026-01-01T00:00:00Z".into(),
        };
        let v: Value = serde_json::to_value(&s).unwrap();
        assert!(v.get("triggerId").is_some());
        assert!(v.get("triggerType").is_some());
        assert!(v.get("positionId").is_some());
        assert!(v.get("occurredAt").is_some());
    }

    #[test]
    fn tool_call_snapshot_is_independent_schema() {
        let s = ToolCallEvidenceSnapshot {
            schema_version: 1,
            name: "fetch_account".into(),
            input_summary: "{}".into(),
            output_summary: Some("ok".into()),
            is_error: false,
            captured_at: "2026-01-01T00:00:00Z".into(),
        };
        let v: Value = serde_json::to_value(&s).unwrap();
        // spec 明确：ToolCallEvidenceSnapshot 不 extend EvidenceSnapshotBase
        assert!(v.get("source").is_none());
        assert!(v.get("freshness").is_none());
        assert!(v.get("inputSummary").is_some());
        assert!(v.get("isError").is_some());
    }
}

// ============ TradeIntent ===============================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeIntentStatus {
    Proposed,
    Submitted,
    Accepted,
    Rejected,
    Executed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountResultRef {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_event_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection_event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeIntent {
    pub intent_id: String,
    pub run_id: String,
    pub episode_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// canonical Account `OperateAccountInput`，JSON 透传。
    pub account_input: serde_json::Value,
    pub reason: String,
    pub strategy_ids: Vec<String>,
    pub status: TradeIntentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_result_ref: Option<AccountResultRef>,
    pub created_at: String,
    pub updated_at: String,
}

// ============ DecisionReview ============================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionReviewTrigger {
    PositionClosed,
    StopLoss,
    TakeProfit,
    TimeStop,
    Invalidated,
    OrderFilled,
    OrderRejected,
    OrderExpired,
    ScheduledReview,
    ManualReview,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecisionReviewResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pnl: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pnl_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_favorable_excursion: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_adverse_excursion: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holding_days: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionReview {
    pub review_id: String,
    pub episode_id: String,
    pub trigger: DecisionReviewTrigger,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<DecisionReviewResult>,
    pub conclusion: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_change: Option<serde_json::Value>,
    pub evidence_refs: Vec<EvidenceRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<crate::domain::shared::WarningCode>,
    pub created_at: String,
}

// ============ StrategyCard ==============================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyStatus {
    Active,
    Paused,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StrategyCardConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entry_rules: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exit_rules: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risk_rules: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub factor_weights: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applicable_regimes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyCard {
    pub strategy_id: String,
    pub version: u32,
    pub name: String,
    pub description: String,
    pub status: StrategyStatus,
    pub config: StrategyCardConfig,
    pub created_at: String,
    pub updated_at: String,
}
