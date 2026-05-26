//! 订阅集行情刷新编排 —— spec `agent-runtime-module.md §5`。
//!
//! Agent Runtime 合并 `Account.subscribed_codes()` + `Quotes.core_indexes()`
//! 后再调用 Quotes canonical `refresh_market_quotes(scope=subscribed)`。
//! Quotes 不反向读 Account / Agent。
//!
//! 现阶段只暴露 `intraday` purpose；`close`（盘后收盘快照）由调度层另行触发。

use std::collections::BTreeSet;

use tauri::AppHandle;

use crate::infrastructure::quotes::core_indexes;
use crate::pipeline::account::subscribed_codes;
use crate::pipeline::market::refresh::{
    refresh_market_quotes, MarketRefreshSummary, RefreshMarketQuotesRequest, RefreshPurpose,
    RefreshScope,
};

/// 合并订阅集（watchlist ∪ open_positions ∪ pending_orders）+ 核心指数后刷新。
pub async fn refresh_subscribed_quotes(
    app: &AppHandle,
) -> Result<MarketRefreshSummary, String> {
    let mut all_set: BTreeSet<String> = BTreeSet::new();
    for ts in subscribed_codes(app) {
        all_set.insert(ts);
    }
    for ts in core_indexes::list() {
        all_set.insert(ts);
    }
    let ts_codes: Vec<String> = all_set.into_iter().collect();
    refresh_market_quotes(
        app,
        RefreshMarketQuotesRequest {
            scope: RefreshScope::Subscribed { ts_codes },
            purpose: RefreshPurpose::Intraday,
            trade_date: None,
        },
    )
    .await
}
