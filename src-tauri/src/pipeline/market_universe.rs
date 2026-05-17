//! 全市场刷新 pipeline——盘中 60s 一轮，分三段（股票 → 指数 → 基金）。
//!
//! ## 设计
//!
//! 每轮：
//! 1. **股票段** —— 按 `|change_percent|` desc 排序（上一轮 MARKET_SNAPSHOT 数据），
//!    热门票优先；SH/SZ 走 TDX 8 连接并行，BJ 走 EM 合流
//! 2. **指数段** —— 自然顺序，TDX 并行
//! 3. **基金段** —— 自然顺序，TDX 并行
//!
//! 每段完成后 emit `market-quotes-refreshed` event，前端逐步看到数据。
//!
//! 与 `market_quote_loop`（active_set，15s）并行运行——
//! universe 给"全市场新鲜度"兜底，active_set 给"我关心的票"高频。

use crate::domain::quotes::StockQuote;
use crate::domain::shared::{Lots, OccurredAt, StockCode, Yuan};
use crate::infrastructure::quotes::realtime::dispatch;
use crate::infrastructure::quotes::realtime::tdx_pool::TdxConnectionPool;
use crate::infrastructure::quotes::snapshot::market_snapshot;
use crate::infrastructure::quotes::tdx::types::{Market, SecurityQuote};
use serde::Serialize;
use serde_json::json;
use std::sync::OnceLock;
use tauri::{AppHandle, Emitter};

const TDX_BATCH_LIMIT: usize = 80;

static POOL: OnceLock<TdxConnectionPool> = OnceLock::new();

fn pool() -> &'static TdxConnectionPool {
    POOL.get_or_init(TdxConnectionPool::new)
}

#[derive(Debug, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct UniverseRefreshSummary {
    pub stock_count: usize,
    pub index_count: usize,
    pub fund_count: usize,
}

/// 一轮 universe 刷新——三段顺序执行。失败仅日志，不抛出。
pub async fn run_universe_refresh(app: &AppHandle) -> UniverseRefreshSummary {
    let mut summary = UniverseRefreshSummary::default();

    // 1. 股票段（按 |change_pct| 倒序，热门优先）
    let stocks_codes = list_stock_ts_codes(app);
    if !stocks_codes.is_empty() {
        let sorted = sort_by_volatility(&stocks_codes);
        summary.stock_count = refresh_category(app, "stock", sorted).await;
    }

    // 2. 指数段
    let index_codes = list_index_ts_codes(app);
    if !index_codes.is_empty() {
        summary.index_count = refresh_category(app, "index", index_codes).await;
    }

    // 3. 基金段
    let fund_codes = list_fund_ts_codes(app);
    if !fund_codes.is_empty() {
        summary.fund_count = refresh_category(app, "fund", fund_codes).await;
    }

    tracing::info!(
        stocks = summary.stock_count,
        indexes = summary.index_count,
        funds = summary.fund_count,
        "universe 刷新完成"
    );
    summary
}

/// 单段刷新：SH/SZ 走 TDX pool 并行，BJ 走 EM；写 MARKET_SNAPSHOT + emit event。
/// 返回成功写入的标的数。
async fn refresh_category(app: &AppHandle, category: &str, codes: Vec<String>) -> usize {
    // 1. 分两类：TDX 能处理的 SH/SZ vs 必须走 EM 的 BJ
    let mut tdx_inputs: Vec<(Market, String, String)> = Vec::with_capacity(codes.len());
    let mut bj_codes: Vec<String> = Vec::new();

    for ts in codes {
        match split_market(&ts) {
            Some((Market::SH, code)) => tdx_inputs.push((Market::SH, code, ts)),
            Some((Market::SZ, code)) => tdx_inputs.push((Market::SZ, code, ts)),
            None if ts.ends_with(".BJ") => bj_codes.push(ts),
            None => continue, // 非法 ts_code 跳过
        }
    }

    // 2. TDX 并行处理 SH/SZ
    let tdx_pairs = if !tdx_inputs.is_empty() {
        let batches: Vec<Vec<(Market, String)>> = tdx_inputs
            .chunks(TDX_BATCH_LIMIT)
            .map(|chunk| chunk.iter().map(|(m, c, _)| (*m, c.clone())).collect())
            .collect();
        let quotes = pool().fetch_batches(batches).await;
        map_tdx_quotes(&tdx_inputs, quotes)
    } else {
        Vec::new()
    };

    // 3. BJ 走 dispatch（TDX 跳过 BJ → 自动回落 EM）
    let bj_pairs = if !bj_codes.is_empty() {
        dispatch().fetch(&bj_codes).await.unwrap_or_else(|e| {
            tracing::warn!(category, err = %e, "universe BJ 段刷新失败");
            Vec::new()
        })
    } else {
        Vec::new()
    };

    // 4. 合并写 MARKET_SNAPSHOT
    let total = tdx_pairs.len() + bj_pairs.len();
    let mut all = tdx_pairs;
    all.extend(bj_pairs);
    market_snapshot::put_batch(all);

    // 5. emit 段级事件
    let _ = app.emit(
        "market-quotes-refreshed",
        json!({
            "category": category,
            "count": total,
            "capturedAt": chrono::Utc::now().to_rfc3339(),
        }),
    );

    total
}

/// 把 TDX 的 SecurityQuote 列表映射回 (ts_code, StockQuote) 对。
fn map_tdx_quotes(
    inputs: &[(Market, String, String)],
    raw: Vec<SecurityQuote>,
) -> Vec<(String, StockQuote)> {
    use std::collections::HashMap;
    let mut ts_lookup: HashMap<(u8, String), String> = HashMap::with_capacity(inputs.len());
    for (m, c, ts) in inputs {
        ts_lookup.insert((m.as_u8(), c.clone()), ts.clone());
    }

    let mut out = Vec::with_capacity(raw.len());
    for q in raw {
        let ts_code = match ts_lookup.get(&(q.market, q.code.clone())) {
            Some(ts) => ts.clone(),
            None => continue,
        };
        let code = match StockCode::new(&q.code) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if q.price <= 0.0 && q.last_close <= 0.0 {
            continue;
        }
        let (change, change_pct) = if q.last_close > 0.0 {
            let c = q.price - q.last_close;
            (
                Some(Yuan::from_unchecked(c)),
                Some(c / q.last_close * 100.0),
            )
        } else {
            (None, None)
        };
        out.push((
            ts_code,
            StockQuote {
                code,
                name: String::new(), // TDX 不返；UI 从 stocks/indexes/funds 表查
                price: if q.price > 0.0 {
                    Some(Yuan::from_unchecked(q.price))
                } else {
                    None
                },
                change_percent: change_pct,
                change,
                open: if q.open > 0.0 {
                    Some(Yuan::from_unchecked(q.open))
                } else {
                    None
                },
                high: if q.high > 0.0 {
                    Some(Yuan::from_unchecked(q.high))
                } else {
                    None
                },
                low: if q.low > 0.0 {
                    Some(Yuan::from_unchecked(q.low))
                } else {
                    None
                },
                previous_close: if q.last_close > 0.0 {
                    Some(Yuan::from_unchecked(q.last_close))
                } else {
                    None
                },
                day_volume: Some(Lots::from_unchecked(q.vol as i64)),
                day_amount: Some(Yuan::from_unchecked(q.amount)),
                captured_at: OccurredAt::now(),
                bid_levels: Vec::new(),
                ask_levels: Vec::new(),
                buy_volume: None,
                sell_volume: None,
                order_imbalance: None,
            },
        ));
    }
    out
}

// ============================================================================
// universe 数据源（按类型分）
// ============================================================================

fn list_stock_ts_codes(app: &AppHandle) -> Vec<String> {
    crate::infrastructure::quotes::repository::list_stocks(app)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            let suffix = match r.market.as_str() {
                "sh" => "SH",
                "sz" => "SZ",
                "bj" => "BJ",
                _ => return None,
            };
            Some(format!("{}.{}", r.code, suffix))
        })
        .collect()
}

fn list_index_ts_codes(app: &AppHandle) -> Vec<String> {
    crate::infrastructure::quotes::repository::list_indexes(app)
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.ts_code)
        .collect()
}

fn list_fund_ts_codes(app: &AppHandle) -> Vec<String> {
    crate::infrastructure::quotes::repository::list_listed_funds(app)
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.ts_code)
        .collect()
}

// ============================================================================
// 排序与映射工具
// ============================================================================

/// 按 |change_percent| 倒序——上一轮 MARKET_SNAPSHOT 没数据的排末尾（首次启动用自然序）。
fn sort_by_volatility(ts_codes: &[String]) -> Vec<String> {
    let mut indexed: Vec<(String, f64)> = ts_codes
        .iter()
        .map(|ts| {
            let abs_pct = market_snapshot::get(ts)
                .and_then(|q| q.change_percent)
                .map(|p| p.abs())
                .unwrap_or(0.0);
            (ts.clone(), abs_pct)
        })
        .collect();
    indexed.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    indexed.into_iter().map(|(ts, _)| ts).collect()
}

/// "000001.SH" → Some((Market::SH, "000001"))；BJ / 非法返 None。
fn split_market(ts_code: &str) -> Option<(Market, String)> {
    if ts_code.len() != 9 || ts_code.as_bytes()[6] != b'.' {
        return None;
    }
    let code = &ts_code[..6];
    match &ts_code[7..] {
        "SH" => Some((Market::SH, code.to_string())),
        "SZ" => Some((Market::SZ, code.to_string())),
        _ => None,
    }
}
