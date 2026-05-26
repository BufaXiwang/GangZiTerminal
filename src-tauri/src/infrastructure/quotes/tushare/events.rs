//! TuShare 公司动作——5 类事件接口适配。
//!
//! - dividend：分红送转
//! - suspend_d：停复牌
//! - namechange：曾用名变更（ST 状态推断）
//! - forecast：业绩预告
//! - share_float：限售股解禁
//!
//! 对齐 spec quotes-module.md §2 `CompanyEvent`：统一 DTO {id, tsCode, eventType,
//! announceDate?, effectiveDate?, payload, source, fetchedAt}；provider 原始字段
//! 进 payload，可审计。

use super::client::{call, row_f64, row_str};
use crate::domain::quotes::{CompanyEvent, CompanyEventType, ForecastType, QuotesError, StStatus};
use crate::domain::shared::{StockCode, TradeDate, TsCode};
use serde_json::{json, Value};
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

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn ts_code_of(code: &StockCode) -> TsCode {
    TsCode::from_unchecked(code.to_ts_code())
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
    let ts_code = ts_code_of(code);
    let source = "tushare:dividend";
    let fetched_at = now_rfc3339();
    Ok(rows
        .iter()
        .filter_map(|row| {
            let announce_date = TradeDate::from_compact(&row_str(row, "ann_date")?).ok()?;
            let ex_date =
                row_str(row, "ex_date").and_then(|s| TradeDate::from_compact(&s).ok());
            let payload: Value = json!({
                "cashPer10": row_f64(row, "cash_div_tax").unwrap_or(0.0) * 10.0,
                "sharePer10": row_f64(row, "stk_div").unwrap_or(0.0) * 10.0,
                "transferPer10": row_f64(row, "stk_bo_rate").unwrap_or(0.0) * 10.0,
                "raw": row,
            });
            let primary = ex_date
                .as_ref()
                .map(|d| d.to_compact())
                .unwrap_or_else(|| announce_date.to_compact());
            let id = CompanyEvent::make_id(
                source,
                ts_code.as_str(),
                CompanyEventType::Dividend,
                Some(&primary),
            );
            Some(CompanyEvent {
                id,
                ts_code: ts_code.clone(),
                event_type: CompanyEventType::Dividend,
                announce_date: Some(announce_date),
                effective_date: ex_date,
                payload,
                source: source.into(),
                fetched_at: fetched_at.clone(),
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
    let ts_code = ts_code_of(code);
    let source = "tushare:suspend_d";
    let fetched_at = now_rfc3339();
    Ok(rows
        .iter()
        .filter_map(|row| {
            let begin = TradeDate::from_compact(&row_str(row, "trade_date")?).ok()?;
            let suspend_type = row_str(row, "suspend_type").unwrap_or_default();
            // TuShare suspend_type "R" = 复牌
            let event_type = if suspend_type == "R" {
                CompanyEventType::Resume
            } else {
                CompanyEventType::Suspension
            };
            let payload = json!({
                "suspendType": suspend_type,
                "suspendTiming": row_str(row, "suspend_timing").unwrap_or_default(),
                "raw": row,
            });
            let primary = begin.to_compact();
            let id =
                CompanyEvent::make_id(source, ts_code.as_str(), event_type, Some(&primary));
            Some(CompanyEvent {
                id,
                ts_code: ts_code.clone(),
                event_type,
                announce_date: Some(begin.clone()),
                effective_date: Some(begin),
                payload,
                source: source.into(),
                fetched_at: fetched_at.clone(),
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
    let ts_code = ts_code_of(code);
    let source = "tushare:namechange";
    let fetched_at = now_rfc3339();
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
            let payload = json!({
                "newStatus": new_status.as_str(),
                "newName": new_name,
                "changeReason": row_str(row, "change_reason").unwrap_or_default(),
                "raw": row,
            });
            let primary = effective_date.to_compact();
            let id = CompanyEvent::make_id(
                source,
                ts_code.as_str(),
                CompanyEventType::St,
                Some(&primary),
            );
            Some(CompanyEvent {
                id,
                ts_code: ts_code.clone(),
                event_type: CompanyEventType::St,
                announce_date: None,
                effective_date: Some(effective_date),
                payload,
                source: source.into(),
                fetched_at: fetched_at.clone(),
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
    let ts_code = ts_code_of(code);
    let source = "tushare:forecast";
    let fetched_at = now_rfc3339();
    Ok(rows
        .iter()
        .filter_map(|row| {
            let announce_date =
                row_str(row, "ann_date").and_then(|s| TradeDate::from_compact(&s).ok());
            let period = row_str(row, "end_date").unwrap_or_default();
            let forecast_type = match row_str(row, "type").as_deref() {
                Some("预增") => ForecastType::Increase,
                Some("预减") => ForecastType::Decrease,
                Some("扭亏") => ForecastType::TurnProfit,
                Some("续亏") | Some("首亏") => ForecastType::TurnLoss,
                Some("续盈") => ForecastType::Continued,
                _ => ForecastType::Unknown,
            };
            let payload = json!({
                "period": period,
                "forecastType": forecast_type.as_str(),
                "minProfit": row_f64(row, "net_profit_min").map(|v| v * 10000.0),
                "maxProfit": row_f64(row, "net_profit_max").map(|v| v * 10000.0),
                "changeMinPct": row_f64(row, "p_change_min"),
                "changeMaxPct": row_f64(row, "p_change_max"),
                "summary": row_str(row, "summary").unwrap_or_default(),
                "raw": row,
            });
            let primary = announce_date
                .as_ref()
                .map(|d| d.to_compact())
                .unwrap_or_else(|| period.clone());
            let id = CompanyEvent::make_id(
                source,
                ts_code.as_str(),
                CompanyEventType::EarningsForecast,
                Some(&primary),
            );
            Some(CompanyEvent {
                id,
                ts_code: ts_code.clone(),
                event_type: CompanyEventType::EarningsForecast,
                announce_date,
                effective_date: None,
                payload,
                source: source.into(),
                fetched_at: fetched_at.clone(),
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
    let ts_code = ts_code_of(code);
    let source = "tushare:share_float";
    let fetched_at = now_rfc3339();
    Ok(rows
        .iter()
        .filter_map(|row| {
            let unlock_date = TradeDate::from_compact(&row_str(row, "float_date")?).ok()?;
            let unlock_shares =
                (row_f64(row, "float_share").unwrap_or(0.0) * 10000.0) as i64;
            let payload = json!({
                "unlockShares": unlock_shares,
                "unlockRatio": row_f64(row, "float_ratio").unwrap_or(0.0),
                "raw": row,
            });
            let primary = unlock_date.to_compact();
            let id = CompanyEvent::make_id(
                source,
                ts_code.as_str(),
                CompanyEventType::Unlock,
                Some(&primary),
            );
            Some(CompanyEvent {
                id,
                ts_code: ts_code.clone(),
                event_type: CompanyEventType::Unlock,
                announce_date: None,
                effective_date: Some(unlock_date),
                payload,
                source: source.into(),
                fetched_at: fetched_at.clone(),
            })
        })
        .collect())
}
