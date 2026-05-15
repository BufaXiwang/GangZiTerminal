//! TuShare 股票 + K 线 + 基本面接口适配。
//!
//! 6 个接口：
//! - `stock_basic`：全市场股票档案
//! - `daily` / `weekly` / `monthly`：日 / 周 / 月线（不复权）
//! - `adj_factor`：复权因子（前复权计算用）
//! - `daily_basic`：每日指标（PE/PB/换手/市值——Phase 4 用）

use super::client::{call, row_f64, row_str};
use crate::domain::quotes::{
    AdjMode, DailyBasic, HistorySource, KlinePeriod, KlinePoint, KlineSeries, QuotesError, StockRef,
};
use crate::domain::shared::{Lots, StockCode, TradeDate, Yuan};
use serde_json::json;
use tauri::AppHandle;

// ============================================================================
// stock_basic——全市场档案
// ============================================================================

/// 拉全市场 A 股档案（上市中）。返回 ~5500 条。
///
/// market 字段直接从 TuShare 返回的 `ts_code` 后缀提取（"000001.SZ" → "sz"），
/// 不再用代码前缀猜测——避免 920xxx（北交所新段）/ B 股代码段被误判。
pub async fn fetch_all_stocks(app: &AppHandle) -> Result<Vec<StockRef>, QuotesError> {
    let params = json!({ "list_status": "L" });
    let rows = call(app, "stock_basic", params, "ts_code,symbol,name,industry").await?;

    let stocks: Vec<StockRef> = rows
        .iter()
        .filter_map(|row| {
            let ts_code = row_str(row, "ts_code")?;
            let mut parts = ts_code.split('.');
            let code_str = parts.next()?.to_string();
            let market = match parts.next()? {
                "SH" => "sh",
                "SZ" => "sz",
                "BJ" => "bj",
                _ => return None,
            }
            .to_string();
            let code = StockCode::new(&code_str).ok()?;
            Some(StockRef {
                code,
                name: row_str(row, "name")?,
                sector: row_str(row, "industry").filter(|s| !s.is_empty()),
                market,
            })
        })
        .collect();

    if stocks.is_empty() {
        return Err(QuotesError::Decode("stock_basic 返回空".into()));
    }
    Ok(stocks)
}

// ============================================================================
// daily / weekly / monthly——历史 K 线
// ============================================================================

/// 拉 K 线（含复权处理）。返回按日期**升序**（旧→新）。
///
/// `limit` 限制返回根数——TuShare 单次最多 6000。
/// `adj`：复权模式。`Qfq` 时会额外调一次 `adj_factor` 接口归一化。
pub async fn fetch_klines(
    app: &AppHandle,
    code: &StockCode,
    period: KlinePeriod,
    limit: usize,
    adj: AdjMode,
) -> Result<KlineSeries, QuotesError> {
    fetch_klines_in_range(app, code, period, limit, adj, None, None).await
}

/// 区间版——`start_date` / `end_date` 形如 "20250101"。
/// 增量刷新场景用：start = last_known_date + 1，end = today。
pub async fn fetch_klines_in_range(
    app: &AppHandle,
    code: &StockCode,
    period: KlinePeriod,
    limit: usize,
    adj: AdjMode,
    start_date: Option<&str>,
    end_date: Option<&str>,
) -> Result<KlineSeries, QuotesError> {
    let ts_code = code.to_ts_code();
    let api_name = match period {
        KlinePeriod::Day => "daily",
        KlinePeriod::Week => "weekly",
        KlinePeriod::Month => "monthly",
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
                amount: Yuan::from_unchecked(row_f64(row, "amount").unwrap_or(0.0) * 1000.0), // 千元→元
            })
        })
        .collect();
    points.sort_by_key(|p| p.date);

    // 限根数
    if points.len() > limit {
        let drop_n = points.len() - limit;
        points.drain(0..drop_n);
    }

    // 前复权
    if matches!(adj, AdjMode::Qfq) {
        apply_qfq(app, code, &mut points).await?;
    }

    Ok(KlineSeries {
        code: code.clone(),
        period,
        adj,
        points,
        source: HistorySource::Tushare,
        stale: false,
        warning: None,
    })
}

// ============================================================================
// adj_factor——前复权因子
// ============================================================================

async fn apply_qfq(
    app: &AppHandle,
    code: &StockCode,
    points: &mut [KlinePoint],
) -> Result<(), QuotesError> {
    if points.is_empty() {
        return Ok(());
    }
    let factors = fetch_adj_factor(app, code).await?;
    if factors.is_empty() {
        return Ok(());
    }
    let latest = factors.last().map(|(_, f)| *f).unwrap_or(1.0);
    let mut fi = 0usize;
    for p in points.iter_mut() {
        while fi + 1 < factors.len() && factors[fi + 1].0 <= p.date {
            fi += 1;
        }
        let f = factors[fi].1;
        let ratio = f / latest;
        p.open = Yuan::from_unchecked(p.open.value() * ratio);
        p.close = Yuan::from_unchecked(p.close.value() * ratio);
        p.high = Yuan::from_unchecked(p.high.value() * ratio);
        p.low = Yuan::from_unchecked(p.low.value() * ratio);
    }
    Ok(())
}

/// 拉 adj_factor 列表，按日期升序。新股 / 不分红股可能为空。
async fn fetch_adj_factor(
    app: &AppHandle,
    code: &StockCode,
) -> Result<Vec<(TradeDate, f64)>, QuotesError> {
    let params = json!({ "ts_code": code.to_ts_code() });
    let rows = call(app, "adj_factor", params, "trade_date,adj_factor").await?;
    let mut out: Vec<(TradeDate, f64)> = rows
        .iter()
        .filter_map(|row| {
            let d = TradeDate::from_compact(&row_str(row, "trade_date")?).ok()?;
            let f = row_f64(row, "adj_factor")?;
            Some((d, f))
        })
        .collect();
    out.sort_by_key(|(d, _)| *d);
    Ok(out)
}

// ============================================================================
// daily_basic——每日基本面指标
// ============================================================================

/// 拉一只票最近一期 daily_basic（PE/PB/换手/市值）。
pub async fn fetch_daily_basic(
    app: &AppHandle,
    code: &StockCode,
) -> Result<Option<DailyBasic>, QuotesError> {
    let params = json!({ "ts_code": code.to_ts_code() });
    let rows = call(
        app,
        "daily_basic",
        params,
        "ts_code,trade_date,pe,pe_ttm,pb,ps,ps_ttm,dv_ratio,dv_ttm,\
         turnover_rate,turnover_rate_f,volume_ratio,total_mv,circ_mv",
    )
    .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let trade_date = TradeDate::from_compact(&row_str(row, "trade_date").unwrap_or_default())?;
    Ok(Some(DailyBasic {
        code: code.clone(),
        trade_date,
        pe: row_f64(row, "pe"),
        pe_ttm: row_f64(row, "pe_ttm"),
        pb: row_f64(row, "pb"),
        ps: row_f64(row, "ps"),
        ps_ttm: row_f64(row, "ps_ttm"),
        dv_ratio: row_f64(row, "dv_ratio"),
        dv_ttm: row_f64(row, "dv_ttm"),
        turnover_rate: row_f64(row, "turnover_rate").unwrap_or(0.0),
        turnover_rate_float: row_f64(row, "turnover_rate_f"),
        volume_ratio: row_f64(row, "volume_ratio").unwrap_or(0.0),
        // TuShare 给的市值是万元——转 Yuan（× 10000）
        total_mv: Yuan::from_unchecked(row_f64(row, "total_mv").unwrap_or(0.0) * 10000.0),
        circ_mv: Yuan::from_unchecked(row_f64(row, "circ_mv").unwrap_or(0.0) * 10000.0),
    }))
}

/// 拉某交易日全市场 daily_basic——给 scanner / 估值筛选用。
pub async fn fetch_daily_basic_by_date(
    app: &AppHandle,
    trade_date: TradeDate,
) -> Result<Vec<DailyBasic>, QuotesError> {
    let params = json!({ "trade_date": trade_date.to_compact() });
    let rows = call(
        app,
        "daily_basic",
        params,
        "ts_code,pe,pe_ttm,pb,ps,ps_ttm,dv_ratio,dv_ttm,\
         turnover_rate,turnover_rate_f,volume_ratio,total_mv,circ_mv",
    )
    .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let ts_code = row_str(row, "ts_code")?;
            let six = ts_code.split('.').next()?;
            let code = StockCode::new(six).ok()?;
            Some(DailyBasic {
                code,
                trade_date,
                pe: row_f64(row, "pe"),
                pe_ttm: row_f64(row, "pe_ttm"),
                pb: row_f64(row, "pb"),
                ps: row_f64(row, "ps"),
                ps_ttm: row_f64(row, "ps_ttm"),
                dv_ratio: row_f64(row, "dv_ratio"),
                dv_ttm: row_f64(row, "dv_ttm"),
                turnover_rate: row_f64(row, "turnover_rate").unwrap_or(0.0),
                turnover_rate_float: row_f64(row, "turnover_rate_f"),
                volume_ratio: row_f64(row, "volume_ratio").unwrap_or(0.0),
                total_mv: Yuan::from_unchecked(row_f64(row, "total_mv").unwrap_or(0.0) * 10000.0),
                circ_mv: Yuan::from_unchecked(row_f64(row, "circ_mv").unwrap_or(0.0) * 10000.0),
            })
        })
        .collect())
}
