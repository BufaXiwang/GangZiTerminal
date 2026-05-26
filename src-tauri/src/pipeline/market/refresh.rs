//! 实时行情刷新——只刷 **订阅集**（Account watchlist + open positions + 核心指数）。
//!
//! 设计：
//! - 数据源：`account::subscribed_codes()` + `quotes::core_indexes()` —— Account subscriptions + 4 大指数
//! - 分批：每批 ≤300 个 secid（EM ulist.np 稳健界）
//! - 并发：3 个 batch 并发（active set 一般 1 批就够，留余量）
//! - 重试：每批失败 retry 2 次，指数退避 500ms / 1500ms
//! - 部分失败容忍：某批彻底失败 → 跳过，不影响其它 batch 写入
//! - 写入：累计成功的 (ts_code, quote) 一次性 put_batch 到 MARKET_SNAPSHOT
//! - emit `market-quotes-refreshed` 给前端
//!
//! 之前是全市场 8497 标的 → 现在 ≤250：EM 单 IP 压力降 95%+。
//!
//! 频率（由 scheduler 决定）：
//! - 盘中：15s（TDX 主路径，16 公共 HQ 服务器分散，无风控压力）
//! - 盘外：60s
//! - 周末/节假日：10min

use crate::domain::shared::TradeDate;
use crate::infrastructure::quotes::realtime::dispatch;
use crate::infrastructure::quotes::snapshot::market_snapshot;
use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// 单 batch 上限——dispatch 内部按 source 拆批（最小 source 是腾讯 60）；
/// 这里只控制总并发的 chunk 粒度，给三源都留余量。
const BATCH_SIZE: usize = 60;
const CONCURRENCY: usize = 3;

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MarketRefreshSummary {
    pub total: usize,
    pub success: usize,
    pub failed_batches: usize,
    pub scope: RefreshScopeKind,
    pub purpose: RefreshPurpose,
}

/// `RefreshMarketQuotesRequest` —— spec `quotes-module.md §4`。
/// scope.kind 决定 ts_codes 来源；调用方必须自行合并订阅集 / 核心指数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RefreshScope {
    /// 订阅集刷新；调用方传入已合并的 watchlist ∪ open_positions ∪ pending_orders ∪ core_indexes
    Subscribed { ts_codes: Vec<String> },
    /// 全市场 universe 刷新
    Universe,
    /// 手动指定的 ts_codes
    Manual { ts_codes: Vec<String> },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshScopeKind {
    Subscribed,
    Universe,
    Manual,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshPurpose {
    Intraday,
    Close,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshMarketQuotesRequest {
    pub scope: RefreshScope,
    pub purpose: RefreshPurpose,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trade_date: Option<TradeDate>,
}

/// canonical refresh —— spec `quotes-module.md §4`。
///
/// 不读取 Account / Agent / News（spec §1 单向依赖）。订阅集合并由调用方
/// （Agent Runtime / scheduler）完成，再传入 `scope`。
pub async fn refresh_market_quotes(
    app: &AppHandle,
    request: RefreshMarketQuotesRequest,
) -> Result<MarketRefreshSummary, String> {
    let scope_kind = match &request.scope {
        RefreshScope::Subscribed { .. } => RefreshScopeKind::Subscribed,
        RefreshScope::Universe => RefreshScopeKind::Universe,
        RefreshScope::Manual { .. } => RefreshScopeKind::Manual,
    };
    let purpose = request.purpose;
    let ts_codes: Vec<String> = match request.scope {
        RefreshScope::Subscribed { ts_codes } => ts_codes,
        RefreshScope::Manual { ts_codes } => {
            if ts_codes.is_empty() {
                return Err("invalid_input: manual scope ts_codes 不能为空".into());
            }
            ts_codes
        }
        RefreshScope::Universe => {
            return Err(
                "universe scope refresh 由 pipeline::market::universe::run_universe_refresh 处理"
                    .into(),
            );
        }
    };
    let total = ts_codes.len();
    if total == 0 {
        tracing::info!("订阅集行情刷新：订阅集为空，跳过");
        return Ok(MarketRefreshSummary {
            total: 0,
            success: 0,
            failed_batches: 0,
            scope: scope_kind,
            purpose,
        });
    }

    // 2. 分批
    let batches: Vec<Vec<String>> = ts_codes.chunks(BATCH_SIZE).map(|c| c.to_vec()).collect();
    let batch_total = batches.len();
    tracing::info!(total, batches = batch_total, "订阅集旁路刷新启动");

    // 3. 并发拉，每批走 dispatch（内部链式 fallback EM > 腾讯 > 新浪）
    let results = stream::iter(batches.into_iter().enumerate())
        .map(|(idx, batch)| async move { fetch_with_retry(idx, &batch).await })
        .buffer_unordered(CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

    // 4. 汇总
    let mut all_pairs: Vec<(String, crate::domain::quotes::StockQuote)> = Vec::with_capacity(total);
    let mut failed_batches = 0;
    for r in results {
        match r {
            Ok(items) => all_pairs.extend(items),
            Err(e) => {
                failed_batches += 1;
                tracing::warn!(err = %e, "某 batch 全失败，跳过");
            }
        }
    }
    let success = all_pairs.len();

    // 5. 写 MARKET_SNAPSHOT + emit (canonical `MarketQuotesRefreshedPayload` per shared-types §6)
    let affected_ts_codes: Vec<String> =
        all_pairs.iter().map(|(ts, _)| ts.clone()).collect();
    market_snapshot::put_batch(all_pairs);
    let scope_str = match scope_kind {
        RefreshScopeKind::Subscribed => "subscribed",
        RefreshScopeKind::Universe => "universe",
        RefreshScopeKind::Manual => "manual",
    };
    let purpose_str = match purpose {
        RefreshPurpose::Intraday => "intraday",
        RefreshPurpose::Close => "close",
    };
    let payload = json!({
        "scope": scope_str,
        "purpose": purpose_str,
        "tradeDate": request.trade_date.map(|d| d.to_compact()),
        "affectedTsCodes": affected_ts_codes,
        "total": total,
        "success": success,
        "failedBatches": failed_batches,
        "capturedAt": chrono::Utc::now().to_rfc3339(),
    });
    let _ = app.emit("market-quotes-refreshed", payload);

    tracing::info!(
        total,
        success,
        failed_batches,
        cache_size = market_snapshot::len(),
        "订阅集行情刷新完成"
    );

    Ok(MarketRefreshSummary {
        total,
        success,
        failed_batches,
        scope: scope_kind,
        purpose,
    })
}


async fn fetch_with_retry(
    idx: usize,
    ts_codes: &[String],
) -> Result<Vec<(String, crate::domain::quotes::StockQuote)>, String> {
    // 请求间小抖动，避免扎堆
    let jitter = (idx % 6) as u64 * 80;
    tokio::time::sleep(Duration::from_millis(jitter)).await;

    let mut last_err: Option<String> = None;
    for attempt in 0..3 {
        match dispatch().fetch(ts_codes).await {
            Ok(items) => return Ok(items),
            Err(e) => {
                last_err = Some(e.to_string());
                if attempt < 2 {
                    let backoff_ms = 500u64.saturating_mul(3u64.pow(attempt));
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| "未知错误".into()))
}

/// 前端首次进今日市场页 hydrate 时拿当前快照（全量 dump）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketQuoteDto {
    pub ts_code: String,
    pub code: String,
    pub name: String,
    pub price: Option<f64>,
    pub change_percent: Option<f64>,
    pub change: Option<f64>,
    pub open: Option<f64>,
    pub high: Option<f64>,
    pub low: Option<f64>,
    pub previous_close: Option<f64>,
    pub volume: Option<f64>,
    pub amount: Option<f64>,
    pub captured_at: i64,
    pub bid_levels: Vec<OrderBookLevelDto>,
    pub ask_levels: Vec<OrderBookLevelDto>,
    pub buy_volume: Option<f64>,
    pub sell_volume: Option<f64>,
    pub order_imbalance: Option<f64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderBookLevelDto {
    pub price: Option<f64>,
    pub volume: Option<f64>,
}

pub async fn snapshot_market_quotes() -> Vec<MarketQuoteDto> {
    market_snapshot::snapshot_all()
        .into_iter()
        .map(quote_to_dto)
        .collect()
}

pub async fn snapshot_market_quotes_for(ts_codes: Vec<String>) -> Vec<MarketQuoteDto> {
    ts_codes
        .into_iter()
        .filter_map(|ts_code| market_snapshot::get(&ts_code).map(|q| quote_to_dto((ts_code, q))))
        .collect()
}

fn quote_to_dto((ts_code, q): (String, crate::domain::quotes::StockQuote)) -> MarketQuoteDto {
    MarketQuoteDto {
        ts_code,
        code: q.code.as_str().to_string(),
        name: q.name,
        price: q.price.map(|v| v.value()),
        change_percent: q.change_percent,
        change: q.change.map(|v| v.value()),
        open: q.open.map(|v| v.value()),
        high: q.high.map(|v| v.value()),
        low: q.low.map(|v| v.value()),
        previous_close: q.previous_close.map(|v| v.value()),
        volume: q.day_volume.map(|v| v.value() as f64),
        amount: q.day_amount.map(|v| v.value()),
        captured_at: q.captured_at.value(),
        bid_levels: q
            .bid_levels
            .into_iter()
            .map(|level| OrderBookLevelDto {
                price: level.price.map(|v| v.value()),
                volume: level.volume.map(|v| v.value() as f64),
            })
            .collect(),
        ask_levels: q
            .ask_levels
            .into_iter()
            .map(|level| OrderBookLevelDto {
                price: level.price.map(|v| v.value()),
                volume: level.volume.map(|v| v.value() as f64),
            })
            .collect(),
        buy_volume: q.buy_volume.map(|v| v.value() as f64),
        sell_volume: q.sell_volume.map(|v| v.value() as f64),
        order_imbalance: q.order_imbalance,
    }
}
