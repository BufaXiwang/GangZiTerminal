//! TuShare 基金——fund_basic + fund_daily。
//!
//! - 档案（fund_basic）：场内基金（ETF / LOF / 封基），给"今日市场"列表用
//! - 日 K（fund_daily）：场内基金二级市场 OHLC + 量额
//!
//! 场外公募（O）数量太大且不在交易系统实时刷，先不拉。

use super::client::{call, row_f64, row_str};
use crate::domain::quotes::{
    AdjMode, HistorySource, KlinePeriod, KlinePoint, KlineSeries, QuotesError,
};
use crate::domain::shared::{Lots, StockCode, TradeDate, Yuan};
use serde_json::json;
use tauri::AppHandle;

/// 一条基金档案——映射 db::FundRow。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundBasic {
    pub ts_code: String,
    pub code: String,
    pub name: String,
    pub market: String,            // E (场内) / O (场外)
    pub fund_type: Option<String>, // 股票型 / 债券型 / 混合型 / 货币型 / FOF / ETF
    pub management: Option<String>,
    pub list_date: Option<String>, // YYYYMMDD
    pub status: Option<String>,    // L / D / I
}

/// 拉场内基金日 K（OHLC + 量额）。
///
/// `ts_code` 形如 "510300.SH"（沪深 300ETF），"159915.SZ"（创业板 ETF）。
/// TuShare 的 fund_daily 接口只支持**日线**——周/月线我们自己用日线聚合（先 placeholder：
/// 周/月也走 fund_daily，由 caller 决定要不要 client-side aggregate）。
///
/// 注意：fund_daily 没有 adj_factor。基金 ETF 的"复权"等价于"考虑分红"，
/// TuShare 的 fund_adj 接口（场外为主）给场内 ETF 的 adj 数据不完整，先不复权。
/// AdjMode 参数被忽略，保留接口对齐。
pub async fn fetch_fund_klines(
    app: &AppHandle,
    ts_code: &str,
    period: KlinePeriod,
    limit: usize,
    adj: AdjMode,
) -> Result<KlineSeries, QuotesError> {
    fetch_fund_klines_in_range(app, ts_code, period, limit, adj, None, None).await
}

pub async fn fetch_fund_klines_in_range(
    app: &AppHandle,
    ts_code: &str,
    period: KlinePeriod,
    limit: usize,
    _adj: AdjMode,
    start_date: Option<&str>,
    end_date: Option<&str>,
) -> Result<KlineSeries, QuotesError> {
    // 周/月 TuShare 没有 fund_weekly/monthly——用 fund_daily 全拉再 caller 聚合。
    if !matches!(period, KlinePeriod::Day) {
        tracing::debug!(
            ts_code = ts_code,
            ?period,
            "fund 暂只支持日 K，周/月降级为日 K 返回"
        );
    }
    let mut params = json!({ "ts_code": ts_code });
    if let Some(s) = start_date {
        params["start_date"] = json!(s);
    }
    if let Some(e) = end_date {
        params["end_date"] = json!(e);
    }
    let rows = call(
        app,
        "fund_daily",
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
                // fund_daily amount 已经是元（不是千元，注意和 daily 区别）
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
        .map_err(|_| QuotesError::InvalidInput(format!("bad fund ts_code: {ts_code}")))?;
    Ok(KlineSeries {
        code,
        period: KlinePeriod::Day, // 强制日 K——周/月 placeholder
        adj: AdjMode::None,
        points,
        source: HistorySource::Tushare,
        stale: false,
        warning: if !matches!(period, KlinePeriod::Day) {
            Some(format!("基金暂不支持 {:?} K 线，显示为日 K", period))
        } else {
            None
        },
    })
}

/// 拉 market = E（场内）+ status = L（上市）的基金。
///
/// 实测 ~700+ 条（包括 ETF / LOF / 封基）。
pub async fn fetch_listed_funds(app: &AppHandle) -> Result<Vec<FundBasic>, QuotesError> {
    let params = json!({
        "market": "E",
        "status": "L",
    });
    let rows = call(
        app,
        "fund_basic",
        params,
        "ts_code,name,management,fund_type,list_date,market,status",
    )
    .await?;
    let items: Vec<FundBasic> = rows
        .iter()
        .filter_map(|row| {
            let ts_code = row_str(row, "ts_code")?;
            let code = ts_code.split('.').next()?.to_string();
            Some(FundBasic {
                ts_code: ts_code.clone(),
                code,
                name: row_str(row, "name")?,
                market: row_str(row, "market").unwrap_or_else(|| "E".into()),
                fund_type: row_str(row, "fund_type"),
                management: row_str(row, "management"),
                list_date: row_str(row, "list_date"),
                status: row_str(row, "status"),
            })
        })
        .collect();
    Ok(items)
}
