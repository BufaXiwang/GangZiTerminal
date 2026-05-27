//! TuShare universe enrich：`stock_basic` / `index_basic` / `fund_basic`。
//!
//! Spec: docs/design/quotes-module.md §5；docs/design/references/quotes/tushare.md

use super::client::{pick_str, TushareClient, TushareError};
use crate::domain::quotes::{InstrumentSource, MarketInstrument};
use crate::domain::shared::{InstrumentCategory, InstrumentStatus, Market, TsCode};
use chrono::Utc;
use serde_json::json;

impl TushareClient {
    /// 全量 A 股 universe（默认拉 listed + suspend；不包括退市，保留旧数据由调用方处理）。
    pub async fn fetch_stock_basic(&self) -> Result<Vec<MarketInstrument>, TushareError> {
        let params = json!({ "list_status": "L" });
        let data = self
            .call(
                "stock_basic",
                params,
                "ts_code,symbol,name,area,industry,market,list_date",
            )
            .await?;
        let now = Utc::now();
        let mut out = Vec::with_capacity(data.items.len());
        for row in data.items.iter() {
            let Some(ts) = pick_str(&data.fields, row, "ts_code") else {
                continue;
            };
            let Ok(ts_code) = TsCode::parse(&ts) else {
                continue;
            };
            let name = pick_str(&data.fields, row, "name").unwrap_or_default();
            let board = pick_str(&data.fields, row, "market");
            let sector = pick_str(&data.fields, row, "industry");
            let list_date = pick_str(&data.fields, row, "list_date");
            let market = ts_code.market();
            let is_st = Some(name.contains("ST"));
            out.push(MarketInstrument {
                ts_code,
                name,
                category: InstrumentCategory::Stock,
                market,
                board,
                sector,
                status: Some(InstrumentStatus::Listed),
                is_st,
                publisher: None,
                index_category: None,
                fund_type: None,
                management: None,
                list_date,
                source: InstrumentSource::Tushare,
                updated_at: now,
            });
        }
        Ok(out)
    }

    /// 标准市场列表（SSE / SZSE / BSE）。spec §2 universe 必须覆盖 SH / SZ / BJ。
    pub fn standard_index_markets() -> &'static [&'static str] {
        &["SSE", "SZSE", "BSE"]
    }

    /// 指数 universe。市场可指定 SSE/SZSE/BSE/CSI，分页可空。
    pub async fn fetch_index_basic(
        &self,
        market_param: &str,
    ) -> Result<Vec<MarketInstrument>, TushareError> {
        let params = json!({ "market": market_param });
        let data = self
            .call(
                "index_basic",
                params,
                "ts_code,name,fullname,market,publisher,category,base_date,list_date",
            )
            .await?;
        let now = Utc::now();
        let mut out = Vec::with_capacity(data.items.len());
        for row in data.items.iter() {
            let Some(ts) = pick_str(&data.fields, row, "ts_code") else {
                continue;
            };
            let ts_code = match TsCode::parse(&ts) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let name = pick_str(&data.fields, row, "name").unwrap_or_default();
            let market_code = ts_code.market();
            // 跳过不在 SH/SZ/BJ 范围内的非 A 股指数（如 CSI、CICC）。
            if !matches!(market_code, Market::SH | Market::SZ | Market::BJ) {
                continue;
            }
            out.push(MarketInstrument {
                ts_code,
                name,
                category: InstrumentCategory::Index,
                market: market_code,
                board: None,
                sector: None,
                status: Some(InstrumentStatus::Listed),
                is_st: None,
                publisher: pick_str(&data.fields, row, "publisher"),
                index_category: pick_str(&data.fields, row, "category"),
                fund_type: None,
                management: None,
                list_date: pick_str(&data.fields, row, "list_date"),
                source: InstrumentSource::Tushare,
                updated_at: now,
            });
        }
        Ok(out)
    }

    /// 场内基金 universe（market = E 表示交易所基金）。
    pub async fn fetch_fund_basic(&self) -> Result<Vec<MarketInstrument>, TushareError> {
        let params = json!({ "market": "E", "status": "L" });
        let data = self
            .call(
                "fund_basic",
                params,
                "ts_code,name,management,fund_type,list_date",
            )
            .await?;
        let now = Utc::now();
        let mut out = Vec::with_capacity(data.items.len());
        for row in data.items.iter() {
            let Some(ts) = pick_str(&data.fields, row, "ts_code") else {
                continue;
            };
            let Ok(ts_code) = TsCode::parse(&ts) else {
                continue;
            };
            let name = pick_str(&data.fields, row, "name").unwrap_or_default();
            let market = ts_code.market();
            if !matches!(market, Market::SH | Market::SZ | Market::BJ) {
                continue;
            }
            out.push(MarketInstrument {
                ts_code,
                name,
                category: InstrumentCategory::Fund,
                market,
                board: None,
                sector: None,
                status: Some(InstrumentStatus::Listed),
                is_st: None,
                publisher: None,
                index_category: None,
                fund_type: pick_str(&data.fields, row, "fund_type"),
                management: pick_str(&data.fields, row, "management"),
                list_date: pick_str(&data.fields, row, "list_date"),
                source: InstrumentSource::Tushare,
                updated_at: now,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_index_markets_covers_sh_sz_bj() {
        let v = TushareClient::standard_index_markets();
        assert_eq!(v.len(), 3);
        assert!(v.contains(&"SSE"));
        assert!(v.contains(&"SZSE"));
        // BSE = 北交所（spec §2 universe 必须覆盖 BJ）
        assert!(v.contains(&"BSE"));
    }
}
