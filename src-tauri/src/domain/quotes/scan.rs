//! Scan 候选筛选 — 字段 / op / sort / filter / ScanResult。
//!
//! Spec: docs/design/quotes-module.md §2 / §4 (scan_market)

use super::quote::{DailyBasic, StockQuote};
use crate::domain::shared::{InstrumentCategory, OccurredAt, TsCode, WarningCode};
use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ScanConditionField {
    ChangePercent,
    Amount,
    Volume,
    TurnoverRate,
    VolumeRatio,
    PeTtm,
    Pb,
    TotalMv,
    CircMv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum ScanOp {
    Gt,
    Gte,
    Lt,
    Lte,
    Eq,
    Between,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(untagged)]
pub enum ScanConditionValue {
    Single(f64),
    Range([f64; 2]),
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ScanCondition {
    pub field: ScanConditionField,
    pub op: ScanOp,
    pub value: ScanConditionValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ScanFilter {
    LimitUp,
    LimitDown,
    TopGain,
    TopLoss,
    TopAmount,
    TopVolume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ScanSortBy {
    ChangePctDesc,
    ChangePctAsc,
    AmountDesc,
    VolumeDesc,
    TurnoverRateDesc,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ScanUniverse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<InstrumentCategory>,
    pub total: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_quote_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_missing_quote_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_expired_quote_count: Option<u32>,
    pub matched: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ScanCriteria {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<ScanCondition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_by: Option<String>,
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ScanItem {
    pub rank: u32,
    pub ts_code: TsCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub category: InstrumentCategory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<StockQuote>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daily_basic: Option<DailyBasic>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<WarningCode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ScanResult {
    pub generated_at: OccurredAt,
    pub universe: ScanUniverse,
    pub criteria: ScanCriteria,
    pub items: Vec<ScanItem>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<WarningCode>,
}
