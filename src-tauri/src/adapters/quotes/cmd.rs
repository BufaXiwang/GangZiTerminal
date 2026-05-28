//! Quotes Tauri commands — `list_market` / `fetch_data` / `scan_market` /
//! `fetch_market_breadth` / `fetch_industry_heatmap`。
//!
//! Spec: docs/design/quotes-module.md §4

use crate::adapters::error::CommandError;
use crate::domain::quotes::{IndustryHeatmap, MarketBreadth};
use crate::pipeline::quotes::service::{
    FetchDataRequest, FetchDataResponse, ListMarketRequest, ListMarketResponse, QuotesService,
    ScanMarketRequest, ScanMarketResponse,
};
use std::sync::Arc;
use tauri::State;

#[tauri::command]
#[specta::specta]
pub fn list_market(
    request: ListMarketRequest,
    service: State<'_, Arc<QuotesService>>,
) -> Result<ListMarketResponse, CommandError> {
    Ok(service.list_market(request))
}

#[tauri::command]
#[specta::specta]
pub fn fetch_data(
    request: FetchDataRequest,
    service: State<'_, Arc<QuotesService>>,
) -> Result<FetchDataResponse, CommandError> {
    Ok(service.fetch_data(request))
}

#[tauri::command]
#[specta::specta]
pub fn scan_market(
    request: ScanMarketRequest,
    service: State<'_, Arc<QuotesService>>,
) -> Result<ScanMarketResponse, CommandError> {
    Ok(service.scan_market(request))
}

/// 市场宽度（spec §4 `market_breadth`）。
///
/// 纯只读聚合，不触发 provider；只统计 `category == stock`。
#[tauri::command]
#[specta::specta]
pub fn fetch_market_breadth(
    service: State<'_, Arc<QuotesService>>,
) -> Result<MarketBreadth, CommandError> {
    Ok(service.market_breadth())
}

/// 行业热度（spec §4 `industry_heatmap`）。
///
/// `top_n` 缺省时使用 5；上限 50（防止 UI 误传大值）。
#[tauri::command]
#[specta::specta]
pub fn fetch_industry_heatmap(
    top_n: Option<u32>,
    service: State<'_, Arc<QuotesService>>,
) -> Result<IndustryHeatmap, CommandError> {
    let n = top_n.unwrap_or(5).clamp(1, 50) as usize;
    Ok(service.industry_heatmap(n))
}
