//! Quotes 跨模块事件 payload。
//!
//! Spec: docs/design/quotes-module.md §5；shared-types.md §6

use crate::domain::shared::{OccurredAt, TradeDate, TsCode};
use serde::{Deserialize, Serialize};
use specta::Type;

/// Spec: shared-types.md §6 `MarketQuotesRefreshedPayload`
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct MarketQuotesRefreshedPayload {
    pub scope: RefreshScopeKind,
    pub purpose: RefreshPurpose,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trade_date: Option<TradeDate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affected_ts_codes: Option<Vec<TsCode>>,
    pub total: u32,
    pub success: u32,
    pub failed_batches: u32,
    pub captured_at: OccurredAt,
}

/// Spec: quotes-module.md §4 — `RefreshMarketQuotesScope` tagged union。
///
/// 实现层把它绑定到 use case；payload 里只保留 kind（外部消费者只需要知道 scope 类型）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RefreshMarketQuotesScope {
    Subscribed {
        #[serde(rename = "tsCodes")]
        ts_codes: Vec<TsCode>,
    },
    Universe,
    Manual {
        #[serde(rename = "tsCodes")]
        ts_codes: Vec<TsCode>,
    },
}

impl RefreshMarketQuotesScope {
    pub fn kind(&self) -> RefreshScopeKind {
        match self {
            RefreshMarketQuotesScope::Subscribed { .. } => RefreshScopeKind::Subscribed,
            RefreshMarketQuotesScope::Universe => RefreshScopeKind::Universe,
            RefreshMarketQuotesScope::Manual { .. } => RefreshScopeKind::Manual,
        }
    }
}

/// Kind 投影，用于事件 payload。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum RefreshScopeKind {
    Subscribed,
    Universe,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum RefreshPurpose {
    Intraday,
    Close,
}

/// 兼容旧名（部分内部代码使用 `RefreshScope`）。
pub use self::RefreshScopeKind as RefreshScope;

/// K-line / daily_basic / events 等 refresh 的 scope；和 quote refresh 共享语义。
///
/// Spec: quotes-module.md §4 `refresh_klines(scope)` / `refresh_daily_basic(scope)` /
/// `refresh_company_events(scope)`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RefreshDataScope {
    Subscribed {
        #[serde(rename = "tsCodes")]
        ts_codes: Vec<TsCode>,
    },
    Universe,
    Manual {
        #[serde(rename = "tsCodes")]
        ts_codes: Vec<TsCode>,
    },
}

/// universe scope 进度事件 payload。
///
/// Spec: shared-types.md §6 `MarketQuotesRefreshProgressPayload`；
/// quotes-module.md §5 "全市场 quote 刷新执行契约" — 每 200 只 emit 一次。
///
/// 消费者只用于增量列表刷新；终态仍以 [`MarketQuotesRefreshedPayload`] 为准。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct MarketQuotesRefreshProgressPayload {
    /// `Universe` = 全市场 60s batch 中间态；`Subscribed` = 热点档 3s 刷新。
    /// 消费者不区分 scope，统一触发增量 UI 刷新。
    pub scope: RefreshScopeKind,
    pub purpose: RefreshPurpose,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trade_date: Option<TradeDate>,
    /// 累计已完成数量（含失败）。
    pub completed: u32,
    /// 累计成功数量。
    pub success: u32,
    /// 本轮目标总数。
    pub total: u32,
    /// 本次 progress 增量写入成功的标的（自上一个 progress 起）。
    pub affected_ts_codes: Vec<TsCode>,
    pub captured_at: OccurredAt,
}

/// Tauri event channel name.
pub const MARKET_QUOTES_REFRESHED_EVENT: &str = "market-quotes-refreshed";
pub const MARKET_QUOTES_REFRESH_PROGRESS_EVENT: &str = "market-quotes-refresh-progress";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_market_quotes_scope_manual_round_trips() {
        let s = RefreshMarketQuotesScope::Manual {
            ts_codes: vec![TsCode::parse("600519.SH").unwrap()],
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: RefreshMarketQuotesScope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.kind(), RefreshScopeKind::Manual);
    }

    #[test]
    fn refresh_market_quotes_scope_universe_round_trips() {
        let json = serde_json::to_string(&RefreshMarketQuotesScope::Universe).unwrap();
        assert!(json.contains("universe"));
        let parsed: RefreshMarketQuotesScope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.kind(), RefreshScopeKind::Universe);
    }

    #[test]
    fn refresh_data_scope_serde() {
        let s = RefreshDataScope::Subscribed {
            ts_codes: vec![TsCode::parse("000001.SZ").unwrap()],
        };
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["kind"], "subscribed");
        assert_eq!(json["tsCodes"][0], "000001.SZ");
    }

    #[test]
    fn refresh_purpose_serde_snake_case() {
        let v = serde_json::to_string(&RefreshPurpose::Close).unwrap();
        assert_eq!(v, "\"close\"");
        let v2 = serde_json::to_string(&RefreshPurpose::Intraday).unwrap();
        assert_eq!(v2, "\"intraday\"");
    }
}
