//! 市场宽度 + 行业热度读模型。
//!
//! Spec: docs/design/quotes-module.md §4 (`market_breadth` / `industry_heatmap`)
//!
//! 纯类型层 —— 不包含 I/O、不依赖 infrastructure / pipeline。
//!
//! ## 涨停 / 跌停阈值规则
//!
//! 复用 [`crate::domain::quotes::compute_limit_band`] 返回的 `LimitBand.up_percent`
//! 作为该标的的适用涨跌幅；不在本模块重复硬编码。
//!
//! 当前 A 股规则（由 `compute_limit_band` 推出）：
//!
//! - 创业板（300 / 301）/ 科创板（688 / 689）：±20%
//! - 北交所（BJ）：±30%
//! - ST 主板（沪 / 深主板 + isSt = true）：±5%
//! - 其他主板：±10%
//! - 指数 / 部分基金：无涨跌停（`bounded = false`），永远不算作涨停 / 跌停
//!
//! 判定方法：使用 `change_percent.abs() >= up_percent - epsilon`（默认
//! `epsilon = 0.05` 百分点）。不依赖精确价格匹配，避免单笔成交在涨停板附近因
//! 浮点 / 撮合 tick 抖动而漏判。

use crate::domain::shared::{OccurredAt, TradeDate, TsCode};
use serde::{Deserialize, Serialize};
use specta::Type;

/// Spec: quotes-module.md §4 `market_breadth`。
///
/// 全市场宽度统计；按 spec 只统计 `category == stock`（不含指数、基金）。
/// 计算前必须先用 quote 有效性规则筛掉无可用 quote 或不匹配 eligible
/// trade date 的标的，这些归入 `no_data`。
#[derive(Clone, Debug, Type, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MarketBreadth {
    /// 有效 quote 的标的总数。`up + down + flat == total`。
    pub total: u32,
    /// `change_percent > 0` 的家数。
    pub up: u32,
    /// `change_percent < 0` 的家数。
    pub down: u32,
    /// `change_percent == 0`（或缺失但 quote 仍有效）的家数。
    pub flat: u32,
    /// 涨停家数（按本模块顶部阈值规则）。是 `up` 的子集。
    pub limit_up: u32,
    /// 跌停家数（按本模块顶部阈值规则）。是 `down` 的子集。
    pub limit_down: u32,
    /// universe 中没有有效 quote 的标的数（snapshot missing / expired / 非 stock category 不计入此数）。
    pub no_data: u32,
    /// 本次统计使用的 eligible trade date。
    pub trade_date: TradeDate,
    /// 计算完成时间。
    pub computed_at: OccurredAt,
}

/// Spec: quotes-module.md §4 `industry_heatmap` 单个行业项。
#[derive(Clone, Debug, Type, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IndustryHeatmapItem {
    /// 行业名称（来自 `MarketInstrument.sector`）。
    pub sector: String,
    /// 该行业内有效 quote 标的的 `change_percent` 算术平均（百分点）。
    pub avg_change_percent: f64,
    /// 该行业参与统计的有效 quote 标的数。
    pub count: u32,
    /// 涨幅 top 3 的 `ts_code`（按 `change_percent desc`；不足 3 个时返回全部）。
    pub leader_codes: Vec<TsCode>,
    /// 与 `leader_codes` 对应的名称。
    pub leader_names: Vec<String>,
}

/// Spec: quotes-module.md §4 `industry_heatmap`。
#[derive(Clone, Debug, Type, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IndustryHeatmap {
    /// 按 `avg_change_percent desc` 取前 N 个行业。
    pub top_gainers: Vec<IndustryHeatmapItem>,
    /// 按 `avg_change_percent asc` 取前 N 个行业。
    pub top_losers: Vec<IndustryHeatmapItem>,
    pub trade_date: TradeDate,
    pub computed_at: OccurredAt,
}
