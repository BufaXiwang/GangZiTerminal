//! Quotes scanner——基于 MARKET_SNAPSHOT + 可选 daily_basic 的复合筛选。
//!
//! scanner 属于 quotes 查询能力：它只读 quotes 自己维护的行情 snapshot、
//! stocks 静态档案和 TuShare daily_basic，不感知 agent/account/news。

use crate::domain::quotes::{
    DailyBasic, QuotesError, ScanCondition, ScanFilter, ScanItem, ScanOp, ScanResult, ScanSort,
    StockQuote,
};
use crate::domain::shared::{OccurredAt, StockCode};
use crate::infrastructure::quotes::snapshot::market_snapshot;
use crate::infrastructure::quotes::tushare::{calendar, stock as ts_stock};
use std::collections::{HashMap, HashSet};
use tauri::AppHandle;

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

#[derive(Debug, Clone)]
struct ScanRow {
    ts_code: String,
    quote: StockQuote,
    name: String,
    basic: Option<DailyBasic>,
}

/// 固定筛选器入口：涨停 / 跌停 / 涨幅 / 跌幅 / 成交额 / 成交量。
pub async fn scan_market(
    app: &AppHandle,
    filter: ScanFilter,
    limit: usize,
) -> Result<ScanResult, QuotesError> {
    let sort_by = match filter {
        ScanFilter::LimitUp | ScanFilter::TopGain => ScanSort::ChangePctDesc,
        ScanFilter::LimitDown | ScanFilter::TopLoss => ScanSort::ChangePctAsc,
        ScanFilter::TopAmount => ScanSort::AmountDesc,
        ScanFilter::TopVolume => ScanSort::VolumeDesc,
    };
    let conditions = match filter {
        ScanFilter::LimitUp => vec![ScanCondition::ChangePct(ScanOp::Gt(9.7))],
        ScanFilter::LimitDown => vec![ScanCondition::ChangePct(ScanOp::Lt(-9.7))],
        _ => Vec::new(),
    };
    scan_market_query(app, conditions, sort_by, limit).await
}

/// 复合查询入口。基本面条件存在时会拉当前交易日 daily_basic 并做内存 join。
pub async fn scan_market_query(
    app: &AppHandle,
    conditions: Vec<ScanCondition>,
    sort_by: ScanSort,
    limit: usize,
) -> Result<ScanResult, QuotesError> {
    let limit = normalize_limit(limit);
    let trade_date = calendar::current_trade_date();
    let needs_basic = conditions.iter().any(condition_needs_basic)
        || matches!(sort_by, ScanSort::TurnoverRateDesc);

    let stock_names = load_stock_names(app)?;
    if stock_names.is_empty() {
        return Err(QuotesError::NotFound(
            "stocks 档案为空，请先刷新全市场档案".into(),
        ));
    }
    let allowed_codes: HashSet<&str> = stock_names.keys().map(|s| s.as_str()).collect();

    let basic_map = if needs_basic {
        match ts_stock::fetch_daily_basic_by_date(app, trade_date).await {
            Ok(items) => items
                .into_iter()
                .map(|b| (b.code.as_str().to_string(), b))
                .collect(),
            Err(e) => {
                tracing::warn!(err = %e, "scanner 拉 daily_basic 失败，基本面字段为空");
                HashMap::new()
            }
        }
    } else {
        HashMap::new()
    };

    let mut rows: Vec<ScanRow> = market_snapshot::snapshot_all()
        .into_iter()
        .filter_map(|(ts_code, quote)| {
            let code = quote.code.as_str().to_string();
            if !allowed_codes.contains(code.as_str()) {
                return None;
            }
            let name = if quote.name.trim().is_empty() {
                stock_names.get(&code).cloned().unwrap_or_default()
            } else {
                quote.name.clone()
            };
            Some(ScanRow {
                ts_code,
                quote,
                name,
                basic: basic_map.get(&code).cloned(),
            })
        })
        .filter(|row| conditions.iter().all(|cond| condition_matches(row, cond)))
        .collect();

    sort_rows(&mut rows, sort_by);

    let captured_at = rows
        .iter()
        .map(|r| r.quote.captured_at)
        .max()
        .unwrap_or_else(OccurredAt::now);

    let items = rows
        .into_iter()
        .take(limit)
        .enumerate()
        .filter_map(|(idx, row)| row_to_item(idx as u32 + 1, row))
        .collect();

    Ok(ScanResult {
        items,
        trade_date,
        captured_at,
        from_cache: true,
    })
}

fn normalize_limit(limit: usize) -> usize {
    if limit == 0 {
        DEFAULT_LIMIT
    } else {
        limit.min(MAX_LIMIT)
    }
}

fn load_stock_names(app: &AppHandle) -> Result<HashMap<String, String>, QuotesError> {
    let rows = crate::infrastructure::quotes::repository::list_stocks(app)
        .map_err(QuotesError::Network)?;
    Ok(rows.into_iter().map(|r| (r.code, r.name)).collect())
}

fn condition_needs_basic(cond: &ScanCondition) -> bool {
    matches!(
        cond,
        ScanCondition::TurnoverRate(_)
            | ScanCondition::VolumeRatio(_)
            | ScanCondition::Pe(_)
            | ScanCondition::Pb(_)
            | ScanCondition::TotalMv(_)
            | ScanCondition::CircMv(_)
    )
}

fn condition_matches(row: &ScanRow, cond: &ScanCondition) -> bool {
    let v = match cond {
        ScanCondition::ChangePct(op) => return match_opt(row.quote.change_percent, *op),
        ScanCondition::Amount(op) => row.quote.day_amount.map(|y| y.value()).map(|v| (v, *op)),
        ScanCondition::Volume(op) => row
            .quote
            .day_volume
            .map(|l| l.value() as f64)
            .map(|v| (v, *op)),
        ScanCondition::TurnoverRate(op) => row.basic.as_ref().map(|b| (b.turnover_rate, *op)),
        ScanCondition::VolumeRatio(op) => row.basic.as_ref().map(|b| (b.volume_ratio, *op)),
        ScanCondition::Pe(op) => row.basic.as_ref().and_then(|b| b.pe.map(|v| (v, *op))),
        ScanCondition::Pb(op) => row.basic.as_ref().and_then(|b| b.pb.map(|v| (v, *op))),
        ScanCondition::TotalMv(op) => row.basic.as_ref().map(|b| (b.total_mv.value(), *op)),
        ScanCondition::CircMv(op) => row.basic.as_ref().map(|b| (b.circ_mv.value(), *op)),
    };
    v.map(|(value, op)| scan_op_matches(value, op))
        .unwrap_or(false)
}

fn match_opt(value: Option<f64>, op: ScanOp) -> bool {
    value.map(|v| scan_op_matches(v, op)).unwrap_or(false)
}

fn scan_op_matches(value: f64, op: ScanOp) -> bool {
    match op {
        ScanOp::Gt(threshold) => value > threshold,
        ScanOp::Lt(threshold) => value < threshold,
        ScanOp::Between(lo, hi) => value >= lo && value <= hi,
    }
}

fn sort_rows(rows: &mut [ScanRow], sort_by: ScanSort) {
    rows.sort_by(|a, b| {
        let av = sort_value(a, sort_by);
        let bv = sort_value(b, sort_by);
        match sort_by {
            ScanSort::ChangePctAsc => av.partial_cmp(&bv).unwrap_or(std::cmp::Ordering::Equal),
            _ => bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal),
        }
        .then_with(|| a.ts_code.cmp(&b.ts_code))
    });
}

fn sort_value(row: &ScanRow, sort_by: ScanSort) -> f64 {
    match sort_by {
        ScanSort::ChangePctDesc | ScanSort::ChangePctAsc => {
            row.quote.change_percent.unwrap_or(f64::NEG_INFINITY)
        }
        ScanSort::AmountDesc => row
            .quote
            .day_amount
            .map(|y| y.value())
            .unwrap_or(f64::NEG_INFINITY),
        ScanSort::VolumeDesc => row
            .quote
            .day_volume
            .map(|l| l.value() as f64)
            .unwrap_or(f64::NEG_INFINITY),
        ScanSort::TurnoverRateDesc => row
            .basic
            .as_ref()
            .map(|b| b.turnover_rate)
            .unwrap_or(f64::NEG_INFINITY),
    }
}

fn row_to_item(rank: u32, row: ScanRow) -> Option<ScanItem> {
    let code = StockCode::new(row.quote.code.as_str()).ok()?;
    Some(ScanItem {
        rank,
        code,
        name: row.name,
        price: row.quote.price,
        change_pct: row.quote.change_percent,
        volume: row.quote.day_volume,
        amount: row.quote.day_amount,
        turnover_rate: row.basic.as_ref().map(|b| b.turnover_rate),
        volume_ratio: row.basic.as_ref().map(|b| b.volume_ratio),
        pe: row.basic.as_ref().and_then(|b| b.pe),
        pb: row.basic.as_ref().and_then(|b| b.pb),
        total_mv: row.basic.map(|b| b.total_mv),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shared::{Lots, Yuan};

    fn quote(code: &str, change_pct: f64, amount: f64, volume: i64) -> StockQuote {
        StockQuote {
            code: StockCode::new(code).unwrap(),
            name: format!("S{code}"),
            price: Some(Yuan::from_unchecked(10.0)),
            change_percent: Some(change_pct),
            change: Some(Yuan::from_unchecked(0.1)),
            open: Some(Yuan::from_unchecked(9.9)),
            high: Some(Yuan::from_unchecked(10.2)),
            low: Some(Yuan::from_unchecked(9.8)),
            previous_close: Some(Yuan::from_unchecked(9.9)),
            day_volume: Some(Lots::from_unchecked(volume)),
            day_amount: Some(Yuan::from_unchecked(amount)),
            captured_at: OccurredAt::new(1000),
            bid_levels: Vec::new(),
            ask_levels: Vec::new(),
            buy_volume: None,
            sell_volume: None,
            order_imbalance: None,
        }
    }

    fn row(ts_code: &str, change_pct: f64, amount: f64, volume: i64) -> ScanRow {
        ScanRow {
            ts_code: ts_code.to_string(),
            quote: quote(&ts_code[..6], change_pct, amount, volume),
            name: ts_code.to_string(),
            basic: None,
        }
    }

    #[test]
    fn scan_op_between_is_inclusive() {
        assert!(scan_op_matches(10.0, ScanOp::Between(9.0, 10.0)));
        assert!(!scan_op_matches(10.1, ScanOp::Between(9.0, 10.0)));
    }

    #[test]
    fn condition_matches_quote_fields() {
        let row = row("600519.SH", 9.9, 120_000_000.0, 42_000);
        assert!(condition_matches(
            &row,
            &ScanCondition::ChangePct(ScanOp::Gt(9.7))
        ));
        assert!(condition_matches(
            &row,
            &ScanCondition::Amount(ScanOp::Gt(100_000_000.0))
        ));
        assert!(!condition_matches(
            &row,
            &ScanCondition::Volume(ScanOp::Lt(10_000.0))
        ));
    }

    #[test]
    fn sort_rows_orders_desc_and_tie_breaks_by_ts_code() {
        let mut rows = vec![
            row("300750.SZ", 3.0, 10.0, 10),
            row("600519.SH", 5.0, 10.0, 10),
            row("000001.SZ", 5.0, 10.0, 10),
        ];
        sort_rows(&mut rows, ScanSort::ChangePctDesc);
        let ordered: Vec<&str> = rows.iter().map(|r| r.ts_code.as_str()).collect();
        assert_eq!(ordered, vec!["000001.SZ", "600519.SH", "300750.SZ"]);
    }
}
