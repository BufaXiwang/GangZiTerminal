//! Quotes Tauri commands — `list_market` / `fetch_data` / `scan_market`。
//!
//! Spec: docs/design/quotes-module.md §4

use crate::adapters::error::CommandError;
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
