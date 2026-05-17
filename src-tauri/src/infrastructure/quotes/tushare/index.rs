//! TuShare 指数日线——index_daily。
//!
//! 给 `get_index_klines` API 用——大盘指数（000001.SH 上证 / 399001.SZ 深证 /
//! 399006.SZ 创业板 / 000688.SH 科创 50）历史 K 线。

use super::client::{call, row_f64, row_str};
use crate::domain::quotes::{
    AdjMode, HistorySource, KlinePeriod, KlinePoint, KlineSeries, MarketIndex, QuotesError,
};
use crate::domain::shared::{Lots, OccurredAt, StockCode, TradeDate, Yuan};
use serde_json::json;
use tauri::AppHandle;

// ============================================================================
// index_basic——指数档案（大盘/行业/主题/风格）
// ============================================================================

/// 一条指数档案——和 crate::infrastructure::quotes::repository::IndexRow 一一对应，但放 domain 层避免循环依赖。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexBasic {
    pub ts_code: String,
    pub code: String,
    pub name: String,
    pub market: String, // SSE / SZSE / CSI / SW
    pub publisher: Option<String>,
    pub category: Option<String>,
}

/// 拉指定 market 的所有指数（"SSE" 上交所 / "SZSE" 深交所 / "CSI" 中证 / "SW" 申万）。
///
/// 默认取主流的 SSE + SZSE + CSI——SW 申万行业指数有 ~100 条，对前端列表展示有用，
/// 用户调用时显式传。
pub async fn fetch_indexes_by_market(
    app: &AppHandle,
    market: &str,
) -> Result<Vec<IndexBasic>, QuotesError> {
    let params = json!({ "market": market });
    let rows = call(
        app,
        "index_basic",
        params,
        "ts_code,name,market,publisher,category",
    )
    .await?;
    let items: Vec<IndexBasic> = rows
        .iter()
        .filter_map(|row| {
            let ts_code = row_str(row, "ts_code")?;
            let code = ts_code.split('.').next()?.to_string();
            Some(IndexBasic {
                ts_code: ts_code.clone(),
                code,
                name: row_str(row, "name")?,
                market: row_str(row, "market").unwrap_or_default(),
                publisher: row_str(row, "publisher"),
                category: row_str(row, "category"),
            })
        })
        .collect();
    Ok(items)
}

/// 拉常用的几个市场——SSE / SZSE / CSI——合并返回。
/// 实测大概返 ~600 个指数（足够日常浏览）。
pub async fn fetch_all_common_indexes(app: &AppHandle) -> Result<Vec<IndexBasic>, QuotesError> {
    let markets = ["SSE", "SZSE", "CSI"];
    let mut all = Vec::new();
    for m in markets {
        match fetch_indexes_by_market(app, m).await {
            Ok(items) => all.extend(items),
            Err(e) => tracing::warn!(market = m, err = %e, "拉指数档案失败，跳过该市场"),
        }
    }
    Ok(all)
}

/// 指数 K 线序列。`ts_code` 形如 "000001.SH"。
pub async fn fetch_index_klines(
    app: &AppHandle,
    ts_code: &str,
    period: KlinePeriod,
    limit: usize,
) -> Result<KlineSeries, QuotesError> {
    fetch_index_klines_in_range(app, ts_code, period, limit, None, None).await
}

/// 区间版——给增量刷新用。
pub async fn fetch_index_klines_in_range(
    app: &AppHandle,
    ts_code: &str,
    period: KlinePeriod,
    limit: usize,
    start_date: Option<&str>,
    end_date: Option<&str>,
) -> Result<KlineSeries, QuotesError> {
    let api_name = match period {
        KlinePeriod::Day => "index_daily",
        KlinePeriod::Week => "index_weekly",
        KlinePeriod::Month => "index_monthly",
    };
    let mut params = json!({ "ts_code": ts_code });
    if let Some(s) = start_date {
        params["start_date"] = json!(s);
    }
    if let Some(e) = end_date {
        params["end_date"] = json!(e);
    }
    let rows = call(
        app,
        api_name,
        params,
        "trade_date,open,high,low,close,vol,amount",
    )
    .await?;

    let mut points: Vec<KlinePoint> = rows
        .iter()
        .filter_map(|row| {
            let trade_date = TradeDate::from_compact(&row_str(row, "trade_date")?).ok()?;
            Some(KlinePoint {
                date: trade_date,
                open: Yuan::from_unchecked(row_f64(row, "open")?),
                close: Yuan::from_unchecked(row_f64(row, "close")?),
                high: Yuan::from_unchecked(row_f64(row, "high")?),
                low: Yuan::from_unchecked(row_f64(row, "low")?),
                volume: Lots::from_unchecked(row_f64(row, "vol").unwrap_or(0.0) as i64),
                amount: Yuan::from_unchecked(row_f64(row, "amount").unwrap_or(0.0) * 1000.0),
            })
        })
        .collect();
    points.sort_by_key(|p| p.date);
    if points.len() > limit {
        let drop_n = points.len() - limit;
        points.drain(0..drop_n);
    }

    let six = ts_code.split('.').next().unwrap_or("");
    let code = StockCode::new(six)
        .map_err(|_| QuotesError::InvalidInput(format!("bad index ts_code: {ts_code}")))?;
    Ok(KlineSeries {
        code,
        period,
        adj: AdjMode::None,
        points,
        source: HistorySource::Tushare,
        stale: false,
        warning: None,
    })
}

/// 拉一只指数的最新一日 snapshot——给 market_overview 用。
pub async fn fetch_index_latest(
    app: &AppHandle,
    ts_code: &str,
    name: &str,
) -> Result<MarketIndex, QuotesError> {
    let params = json!({ "ts_code": ts_code });
    let rows = call(
        app,
        "index_daily",
        params,
        "trade_date,close,pct_chg,change",
    )
    .await?;
    let first = rows
        .first()
        .ok_or_else(|| QuotesError::NotFound(format!("index {ts_code} 无数据")))?;
    let close = row_f64(first, "close")
        .ok_or_else(|| QuotesError::Decode("index_daily 缺 close".into()))?;
    let six = ts_code.split('.').next().unwrap_or("");
    let code = StockCode::new(six)
        .map_err(|_| QuotesError::InvalidInput(format!("bad index ts_code: {ts_code}")))?;
    Ok(MarketIndex {
        code,
        name: name.to_string(),
        price: Some(Yuan::from_unchecked(close)),
        change: row_f64(first, "change").map(Yuan::from_unchecked),
        change_percent: row_f64(first, "pct_chg"),
        timestamp: OccurredAt::now(),
    })
}
