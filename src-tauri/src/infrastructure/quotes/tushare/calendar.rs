//! TuShare `trade_cal` 拉取。
//!
//! Spec: docs/design/quotes-module.md §5；docs/design/references/quotes/tushare.md

use super::client::{pick_i64, pick_str, TushareClient, TushareError};
use crate::domain::shared::TradeDate;
use serde_json::json;

pub struct CalendarEntry {
    pub cal_date: TradeDate,
    pub is_open: bool,
    pub pretrade_date: Option<TradeDate>,
}

impl TushareClient {
    /// 拉取 `exchange = SSE` 的交易日历（沪深节假日一致）。
    pub async fn fetch_trade_cal(
        &self,
        start_date: &str,
        end_date: &str,
    ) -> Result<Vec<CalendarEntry>, TushareError> {
        let params = json!({
            "exchange": "SSE",
            "start_date": start_date,
            "end_date": end_date,
        });
        let data = self
            .call(
                "trade_cal",
                params,
                "cal_date,is_open,pretrade_date",
            )
            .await?;
        let mut out = Vec::with_capacity(data.items.len());
        for row in data.items.iter() {
            let Some(date_s) = pick_str(&data.fields, row, "cal_date") else {
                continue;
            };
            let Ok(cal_date) = TradeDate::parse(&date_s) else {
                continue;
            };
            let is_open = pick_i64(&data.fields, row, "is_open")
                .map(|n| n != 0)
                .unwrap_or(false);
            let pretrade_date = pick_str(&data.fields, row, "pretrade_date")
                .and_then(|s| TradeDate::parse(&s).ok());
            out.push(CalendarEntry {
                cal_date,
                is_open,
                pretrade_date,
            });
        }
        Ok(out)
    }
}
