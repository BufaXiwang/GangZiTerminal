//! MarketInstrument / StockProfile — Quotes 统一标的模型。
//!
//! Spec: docs/design/quotes-module.md §2

use crate::domain::shared::{InstrumentCategory, InstrumentStatus, Market, OccurredAt, TsCode};
use serde::{Deserialize, Serialize};
use specta::Type;

/// Spec: quotes-module.md §2 统一标的模型
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct MarketInstrument {
    pub ts_code: TsCode,
    pub name: String,
    pub category: InstrumentCategory,
    pub market: Market,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub board: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<InstrumentStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_st: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fund_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub management: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_date: Option<String>,
    pub source: InstrumentSource,
    pub updated_at: OccurredAt,
}

/// Spec: quotes-module.md §2 — universe 来源。
///
/// `Builtin` 表示 cold-start seed 行：在 process startup 时由 `seed_builtin_instruments`
/// 写入，仅作 diagnostic 用途。真实 provider refresh 完成后 source 会被覆盖（spec §5 step 0）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum InstrumentSource {
    Builtin,
    Tdx,
    Eastmoney,
    Tushare,
    Mixed,
}

/// Spec: quotes-module.md §2 — fetch_data profile 投影。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct StockProfile {
    pub ts_code: TsCode,
    pub name: String,
    pub category: InstrumentCategory,
    pub market: Market,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub board: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<InstrumentStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_st: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_date: Option<String>,
}

impl From<&MarketInstrument> for StockProfile {
    fn from(inst: &MarketInstrument) -> Self {
        Self {
            ts_code: inst.ts_code.clone(),
            name: inst.name.clone(),
            category: inst.category,
            market: inst.market,
            board: inst.board.clone(),
            sector: inst.sector.clone(),
            status: inst.status,
            is_st: inst.is_st,
            list_date: inst.list_date.clone(),
        }
    }
}
