//! TuShare 资金面接口——龙虎榜 / 资金流向 / 北向 / 融资融券。

use super::client::{call, row_f64, row_str};
use crate::domain::quotes::{
    MarginSummary, MoneyFlowItem, NorthHolding, NorthMoneyFlow, QuotesError, TopListItem,
};
use crate::domain::shared::{StockCode, TradeDate, Yuan};
use serde_json::json;
use tauri::AppHandle;

// ============================================================================
// top_list 龙虎榜
// ============================================================================

pub async fn fetch_top_list(
    app: &AppHandle,
    trade_date: Option<TradeDate>,
) -> Result<Vec<TopListItem>, QuotesError> {
    let params = match trade_date {
        Some(d) => json!({ "trade_date": d.to_compact() }),
        None => json!({}),
    };
    let rows = call(
        app,
        "top_list",
        params,
        "trade_date,ts_code,name,close,pct_change,turnover_rate,amount,net_amount,net_rate,reason",
    )
    .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let trade_date = TradeDate::from_compact(&row_str(row, "trade_date")?).ok()?;
            let ts_code = row_str(row, "ts_code")?;
            let six = ts_code.split('.').next()?;
            let code = StockCode::new(six).ok()?;
            Some(TopListItem {
                trade_date,
                code,
                name: row_str(row, "name").unwrap_or_default(),
                close: row_f64(row, "close").map(Yuan::from_unchecked),
                pct_change: row_f64(row, "pct_change"),
                turnover_rate: row_f64(row, "turnover_rate"),
                amount: row_f64(row, "amount").map(Yuan::from_unchecked),
                net_amount: row_f64(row, "net_amount").map(Yuan::from_unchecked),
                net_rate: row_f64(row, "net_rate"),
                reason: row_str(row, "reason").unwrap_or_default(),
            })
        })
        .collect())
}

// ============================================================================
// moneyflow 个股资金流
// ============================================================================

pub async fn fetch_moneyflow(
    app: &AppHandle,
    code: &StockCode,
    days: usize,
) -> Result<Vec<MoneyFlowItem>, QuotesError> {
    let params = json!({ "ts_code": code.to_ts_code() });
    let rows = call(
        app,
        "moneyflow",
        params,
        "ts_code,trade_date,buy_sm_amount,sell_sm_amount,\
         buy_md_amount,sell_md_amount,buy_lg_amount,sell_lg_amount,\
         buy_elg_amount,sell_elg_amount,net_mf_amount",
    )
    .await?;
    let mut out: Vec<MoneyFlowItem> = rows
        .iter()
        .filter_map(|row| {
            let trade_date = TradeDate::from_compact(&row_str(row, "trade_date")?).ok()?;
            let net = |buy: &str, sell: &str| match (row_f64(row, buy), row_f64(row, sell)) {
                (Some(b), Some(s)) => Some(Yuan::from_unchecked(b - s)),
                _ => None,
            };
            Some(MoneyFlowItem {
                trade_date,
                code: code.clone(),
                net_small: net("buy_sm_amount", "sell_sm_amount"),
                net_mid: net("buy_md_amount", "sell_md_amount"),
                net_large: net("buy_lg_amount", "sell_lg_amount"),
                net_extra_large: net("buy_elg_amount", "sell_elg_amount"),
                net_total: row_f64(row, "net_mf_amount").map(Yuan::from_unchecked),
            })
        })
        .collect();
    out.sort_by_key(|m| m.trade_date);
    if out.len() > days {
        out.drain(0..out.len() - days);
    }
    Ok(out)
}

// ============================================================================
// 北向资金（sh / sz 整体净流入）
// ============================================================================

pub async fn fetch_north_flow(
    app: &AppHandle,
    days: usize,
) -> Result<Vec<NorthMoneyFlow>, QuotesError> {
    let params = json!({});
    let rows = call(
        app,
        "moneyflow_hsgt",
        params,
        "trade_date,hgt,sgt,north_money",
    )
    .await?;
    let mut out: Vec<NorthMoneyFlow> = rows
        .iter()
        .filter_map(|row| {
            let trade_date = TradeDate::from_compact(&row_str(row, "trade_date")?).ok()?;
            // hgt / sgt 单位是百万元（TuShare 文档）—— 转元
            let sh_north = Yuan::from_unchecked(row_f64(row, "hgt").unwrap_or(0.0) * 1_000_000.0);
            let sz_north = Yuan::from_unchecked(row_f64(row, "sgt").unwrap_or(0.0) * 1_000_000.0);
            let total = Yuan::from_unchecked(
                row_f64(row, "north_money").unwrap_or(sh_north.value() + sz_north.value())
                    * if row_f64(row, "north_money").is_some() {
                        1_000_000.0
                    } else {
                        1.0
                    },
            );
            Some(NorthMoneyFlow {
                trade_date,
                sh_north,
                sz_north,
                total,
            })
        })
        .collect();
    out.sort_by_key(|f| f.trade_date);
    if out.len() > days {
        out.drain(0..out.len() - days);
    }
    Ok(out)
}

// ============================================================================
// 北向 top10 个股
// ============================================================================

pub async fn fetch_north_top10(
    app: &AppHandle,
    trade_date: TradeDate,
) -> Result<Vec<NorthHolding>, QuotesError> {
    let params = json!({ "trade_date": trade_date.to_compact() });
    let rows = call(
        app,
        "hsgt_top10",
        params,
        "ts_code,name,trade_date,amount,hold_ratio",
    )
    .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let td = TradeDate::from_compact(&row_str(row, "trade_date")?).ok()?;
            let ts_code = row_str(row, "ts_code")?;
            let six = ts_code.split('.').next()?;
            let code = StockCode::new(six).ok()?;
            Some(NorthHolding {
                trade_date: td,
                code,
                name: row_str(row, "name").unwrap_or_default(),
                hold_amount: row_f64(row, "amount").map(Yuan::from_unchecked),
                hold_ratio: row_f64(row, "hold_ratio"),
            })
        })
        .collect())
}

// ============================================================================
// 融资融券每日汇总
// ============================================================================

pub async fn fetch_margin_summary(
    app: &AppHandle,
    days: usize,
) -> Result<Vec<MarginSummary>, QuotesError> {
    let params = json!({});
    let rows = call(app, "margin", params, "trade_date,rzye,rqye,rzmre,rqmcl").await?;
    let mut out: Vec<MarginSummary> = rows
        .iter()
        .filter_map(|row| {
            let trade_date = TradeDate::from_compact(&row_str(row, "trade_date")?).ok()?;
            Some(MarginSummary {
                trade_date,
                financing_balance: Yuan::from_unchecked(row_f64(row, "rzye").unwrap_or(0.0)),
                margin_balance: Yuan::from_unchecked(row_f64(row, "rqye").unwrap_or(0.0)),
                financing_buy: Yuan::from_unchecked(row_f64(row, "rzmre").unwrap_or(0.0)),
                margin_sell: Yuan::from_unchecked(row_f64(row, "rqmcl").unwrap_or(0.0)),
            })
        })
        .collect();
    out.sort_by_key(|m| m.trade_date);
    if out.len() > days {
        out.drain(0..out.len() - days);
    }
    Ok(out)
}
