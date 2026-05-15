//! TuShare 概念板块——concept / concept_detail。
//!
//! - `fetch_concept_list`：所有概念板块清单
//! - `fetch_concept_members`：板块成分股
//! - `fetch_concept_performance`：板块涨跌榜（基于成员当日 daily 聚合）

use super::client::{call, row_f64, row_str};
use crate::domain::quotes::{ConceptPerformance, ConceptSector, QuotesError};
use crate::domain::shared::{StockCode, TradeDate, Yuan};
use serde_json::json;
use std::collections::HashMap;
use tauri::AppHandle;

/// 所有概念板块清单。
pub async fn fetch_concept_list(app: &AppHandle) -> Result<Vec<ConceptSector>, QuotesError> {
    let params = json!({});
    let rows = call(app, "concept", params, "code,name").await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some(ConceptSector {
                code: row_str(row, "code")?,
                name: row_str(row, "name")?,
                member_count: None,
            })
        })
        .collect())
}

/// 板块成分股（concept_code → list of StockCode）。
pub async fn fetch_concept_members(
    app: &AppHandle,
    concept_code: &str,
) -> Result<Vec<StockCode>, QuotesError> {
    let params = json!({ "id": concept_code });
    let rows = call(app, "concept_detail", params, "ts_code").await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let ts_code = row_str(row, "ts_code")?;
            let six = ts_code.split('.').next()?;
            StockCode::new(six).ok()
        })
        .collect())
}

/// 板块涨跌榜——基于某交易日全市场 daily 数据 + 概念成分聚合。
///
/// 实现：拉全部 concept_detail → 拉 trade_date 全市场 daily → 按板块聚合涨跌幅平均。
/// 调用成本较高，建议 caller 加 cache。
pub async fn fetch_concept_performance(
    app: &AppHandle,
    trade_date: TradeDate,
) -> Result<Vec<ConceptPerformance>, QuotesError> {
    // Step 1: 拉所有概念
    let concepts = fetch_concept_list(app).await?;
    if concepts.is_empty() {
        return Ok(Vec::new());
    }

    // Step 2: 拉全市场 daily（按 trade_date）
    let daily_params = json!({ "trade_date": trade_date.to_compact() });
    let daily_rows = call(
        app,
        "daily",
        daily_params,
        "ts_code,close,pct_chg,vol,amount",
    )
    .await?;
    let mut daily_map: HashMap<StockCode, (f64, f64)> = HashMap::new(); // code → (pct_chg, amount-千元)
    for row in &daily_rows {
        if let Some(ts_code) = row_str(row, "ts_code") {
            if let Some(six) = ts_code.split('.').next() {
                if let Ok(code) = StockCode::new(six) {
                    let pct = row_f64(row, "pct_chg").unwrap_or(0.0);
                    let amt = row_f64(row, "amount").unwrap_or(0.0);
                    daily_map.insert(code, (pct, amt));
                }
            }
        }
    }

    // Step 3: 每个 concept 聚合成员
    let mut out = Vec::with_capacity(concepts.len());
    for concept in concepts {
        let members = fetch_concept_members(app, &concept.code)
            .await
            .unwrap_or_default();
        let mut total_pct = 0.0;
        let mut total_amt = 0.0;
        let mut count = 0;
        let mut leader: Option<(StockCode, f64)> = None;
        for m in &members {
            if let Some((pct, amt)) = daily_map.get(m) {
                total_pct += pct;
                total_amt += amt;
                count += 1;
                if leader.as_ref().map(|(_, p)| *p < *pct).unwrap_or(true) {
                    leader = Some((m.clone(), *pct));
                }
            }
        }
        if count > 0 {
            out.push(ConceptPerformance {
                code: concept.code,
                name: concept.name,
                trade_date,
                avg_change_pct: total_pct / count as f64,
                total_amount: Yuan::from_unchecked(total_amt * 1000.0), // 千元 → 元
                leader: leader.as_ref().map(|(c, _)| c.clone()),
                leader_change_pct: leader.as_ref().map(|(_, p)| *p),
            });
        }
    }

    // 按涨幅降序
    out.sort_by(|a, b| {
        b.avg_change_pct
            .partial_cmp(&a.avg_change_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}
