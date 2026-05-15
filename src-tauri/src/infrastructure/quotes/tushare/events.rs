//! TuShare 公司动作——5 类事件接口适配。
//!
//! - dividend：分红送转
//! - suspend_d：停复牌
//! - namechange：曾用名变更（ST 状态推断）
//! - forecast：业绩预告
//! - share_float：限售股解禁

use super::client::{call, row_f64, row_i64, row_str};
use crate::domain::quotes::{CompanyEvent, ForecastType, QuotesError, StStatus};
use crate::domain::shared::{Lots, StockCode, TradeDate, Yuan};
use serde_json::json;
use tauri::AppHandle;

/// 拉一只票的近期公司动作——多接口合并，按日期降序。
pub async fn fetch_company_events(
    app: &AppHandle,
    code: &StockCode,
    days_ahead: i32,
) -> Result<Vec<CompanyEvent>, QuotesError> {
    let mut events = Vec::new();
    events.extend(fetch_dividend(app, code).await?);
    events.extend(fetch_suspension(app, code).await?);
    events.extend(fetch_name_change(app, code).await?);
    events.extend(fetch_forecast(app, code).await?);
    events.extend(fetch_share_float(app, code, days_ahead).await?);
    Ok(events)
}

async fn fetch_dividend(
    app: &AppHandle,
    code: &StockCode,
) -> Result<Vec<CompanyEvent>, QuotesError> {
    let params = json!({ "ts_code": code.to_ts_code() });
    let rows = call(
        app,
        "dividend",
        params,
        "ann_date,ex_date,cash_div_tax,stk_div,stk_bo_rate",
    )
    .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let announce_date = TradeDate::from_compact(&row_str(row, "ann_date")?).ok()?;
            let ex_date = row_str(row, "ex_date").and_then(|s| TradeDate::from_compact(&s).ok());
            Some(CompanyEvent::Dividend {
                announce_date,
                ex_date,
                cash_per_10: row_f64(row, "cash_div_tax").unwrap_or(0.0) * 10.0, // 每股 → 每 10 股
                share_per_10: row_f64(row, "stk_div").unwrap_or(0.0) * 10.0,
                transfer_per_10: row_f64(row, "stk_bo_rate").unwrap_or(0.0) * 10.0,
            })
        })
        .collect())
}

async fn fetch_suspension(
    app: &AppHandle,
    code: &StockCode,
) -> Result<Vec<CompanyEvent>, QuotesError> {
    let params = json!({ "ts_code": code.to_ts_code() });
    let rows = call(
        app,
        "suspend_d",
        params,
        "trade_date,suspend_type,suspend_timing",
    )
    .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let begin = TradeDate::from_compact(&row_str(row, "trade_date")?).ok()?;
            Some(CompanyEvent::Suspension {
                begin_date: begin,
                end_date: None,
                reason: row_str(row, "suspend_timing").unwrap_or_default(),
            })
        })
        .collect())
}

async fn fetch_name_change(
    app: &AppHandle,
    code: &StockCode,
) -> Result<Vec<CompanyEvent>, QuotesError> {
    let params = json!({ "ts_code": code.to_ts_code() });
    let rows = call(app, "namechange", params, "name,start_date,change_reason").await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let effective_date = TradeDate::from_compact(&row_str(row, "start_date")?).ok()?;
            let new_name = row_str(row, "name").unwrap_or_default();
            let new_status = if new_name.contains("*ST") {
                StStatus::StarSt
            } else if new_name.contains("ST") {
                StStatus::St
            } else if new_name.contains("退") {
                StStatus::Delisted
            } else {
                StStatus::Normal
            };
            Some(CompanyEvent::StChange {
                effective_date,
                new_status,
                previous_name: row_str(row, "change_reason").unwrap_or_default(),
                new_name,
            })
        })
        .collect())
}

async fn fetch_forecast(
    app: &AppHandle,
    code: &StockCode,
) -> Result<Vec<CompanyEvent>, QuotesError> {
    let params = json!({ "ts_code": code.to_ts_code() });
    let rows = call(
        app,
        "forecast",
        params,
        "ann_date,end_date,type,p_change_min,p_change_max,net_profit_min,net_profit_max,summary",
    )
    .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let period = row_str(row, "end_date").unwrap_or_default();
            let forecast_type = match row_str(row, "type").as_deref() {
                Some("预增") => ForecastType::Increase,
                Some("预减") => ForecastType::Decrease,
                Some("扭亏") => ForecastType::TurnProfit,
                Some("续亏") | Some("首亏") => ForecastType::TurnLoss,
                Some("续盈") => ForecastType::Continued,
                _ => ForecastType::Unknown,
            };
            Some(CompanyEvent::EarningsForecast {
                period,
                forecast_type,
                min_profit: row_f64(row, "net_profit_min")
                    .map(|v| Yuan::from_unchecked(v * 10000.0)),
                max_profit: row_f64(row, "net_profit_max")
                    .map(|v| Yuan::from_unchecked(v * 10000.0)),
                change_min_pct: row_f64(row, "p_change_min"),
                change_max_pct: row_f64(row, "p_change_max"),
                summary: row_str(row, "summary").unwrap_or_default(),
            })
        })
        .collect())
}

async fn fetch_share_float(
    app: &AppHandle,
    code: &StockCode,
    _days_ahead: i32,
) -> Result<Vec<CompanyEvent>, QuotesError> {
    let params = json!({ "ts_code": code.to_ts_code() });
    let rows = call(
        app,
        "share_float",
        params,
        "float_date,float_share,float_ratio",
    )
    .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let unlock_date = TradeDate::from_compact(&row_str(row, "float_date")?).ok()?;
            Some(CompanyEvent::ShareUnlock {
                unlock_date,
                unlock_shares: Lots::from_unchecked(
                    (row_f64(row, "float_share").unwrap_or(0.0) * 10000.0) as i64 / 100,
                ),
                unlock_ratio: row_f64(row, "float_ratio").unwrap_or(0.0),
            })
        })
        .collect())
}
