//! Quotes Tauri commands — list_market / fetch_data / scan_market /
//! fetch_market_breadth / fetch_industry_heatmap / ensure_chart_data。
//!
//! Spec: docs/design/quotes-module.md §4

use crate::adapters::error::CommandError;
use crate::domain::quotes::{
    IndustryHeatmap, KlinePeriod, MarketBreadth, MinuteKlinePeriod, RefreshDataScope,
};
use crate::domain::shared::{ErrorCode, TsCode};
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

/// 前端 on-demand 拉数据：用户选中标的 + 切到某 chart period 时，如果 DB 空就触发后端拉一份。
///
/// 按 period 字符串分派：
/// - `"day"` / `"week"` / `"month"` → `refresh_klines`
/// - `"1m"` / `"5m"` / `"15m"` / `"30m"` / `"60m"` → `refresh_minute_klines`
/// - `"intraday"` → `refresh_intraday`
///
/// 同步等待 refresh 完成；调用方拿到 Ok 后可以再次调用 fetch_data 拿数据。
///
/// 已有数据时也会重新拉（refresh 是 upsert，幂等）；UI 调用方决定是否要重新触发。
#[tauri::command]
#[specta::specta]
pub async fn ensure_chart_data(
    ts_code: String,
    period: String,
    service: State<'_, Arc<QuotesService>>,
) -> Result<(), CommandError> {
    let code = TsCode::parse(&ts_code)
        .map_err(|e| CommandError::with_message(ErrorCode::InvalidInput, e.to_string()))?;
    let scope = RefreshDataScope::Subscribed {
        ts_codes: vec![code],
    };
    match period.as_str() {
        "day" => {
            service.refresh_klines(scope, vec![KlinePeriod::Day]).await?;
        }
        "week" => {
            service.refresh_klines(scope, vec![KlinePeriod::Week]).await?;
        }
        "month" => {
            service
                .refresh_klines(scope, vec![KlinePeriod::Month])
                .await?;
        }
        "1m" => {
            service
                .refresh_minute_klines(scope, vec![MinuteKlinePeriod::M1])
                .await?;
        }
        "5m" => {
            service
                .refresh_minute_klines(scope, vec![MinuteKlinePeriod::M5])
                .await?;
        }
        "15m" => {
            service
                .refresh_minute_klines(scope, vec![MinuteKlinePeriod::M15])
                .await?;
        }
        "30m" => {
            service
                .refresh_minute_klines(scope, vec![MinuteKlinePeriod::M30])
                .await?;
        }
        "60m" => {
            service
                .refresh_minute_klines(scope, vec![MinuteKlinePeriod::M60])
                .await?;
        }
        "intraday" => {
            service.refresh_intraday(scope).await?;
        }
        _ => {
            return Err(CommandError::with_message(
                ErrorCode::InvalidInput,
                format!("unknown period: {}", period),
            ));
        }
    }
    Ok(())
}
