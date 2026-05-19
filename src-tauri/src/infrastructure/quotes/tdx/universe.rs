//! TDX security list fallback for local market reference tables.
//!
//! `security_list` returns the whole exchange quote namespace, not a clean
//! stock_basic-style catalog. Keep the filters conservative: only emit rows
//! whose category is determined by high-confidence code prefixes.

use crate::domain::quotes::QuotesError;
use crate::infrastructure::quotes::tdx::client::TdxHqClient;
use crate::infrastructure::quotes::tdx::types::{Market, SecurityListEntry};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default)]
pub struct TdxUniverse {
    pub stocks: Vec<TdxStockRef>,
    pub indexes: Vec<TdxIndexRef>,
    pub funds: Vec<TdxFundRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TdxStockRef {
    pub code: String,
    pub name: String,
    pub market: String, // sh / sz
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TdxIndexRef {
    pub ts_code: String,
    pub code: String,
    pub name: String,
    pub market: String, // SSE / SZSE
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TdxFundRef {
    pub ts_code: String,
    pub code: String,
    pub name: String,
    pub fund_type: Option<String>,
}

pub async fn fetch_universe() -> Result<TdxUniverse, QuotesError> {
    tokio::task::spawn_blocking(fetch_universe_blocking)
        .await
        .map_err(|e| QuotesError::Network(format!("TDX universe task join: {e}")))?
}

fn fetch_universe_blocking() -> Result<TdxUniverse, QuotesError> {
    let (mut client, peer) = TdxHqClient::connect_bestip(CONNECT_TIMEOUT)
        .map_err(|e| QuotesError::Network(format!("TDX 建连失败：{e}")))?;
    tracing::info!(peer = %peer, "TDX universe 连接建立");

    let mut out = TdxUniverse::default();
    collect_market(&mut client, Market::SH, &mut out)?;
    collect_market(&mut client, Market::SZ, &mut out)?;

    if out.stocks.is_empty() && out.indexes.is_empty() && out.funds.is_empty() {
        return Err(QuotesError::Decode(
            "TDX security_list 未解析出任何可用档案".into(),
        ));
    }
    tracing::info!(
        stocks = out.stocks.len(),
        indexes = out.indexes.len(),
        funds = out.funds.len(),
        "TDX universe 解析完成"
    );
    Ok(out)
}

fn collect_market(
    client: &mut TdxHqClient,
    market: Market,
    out: &mut TdxUniverse,
) -> Result<(), QuotesError> {
    let count = client
        .security_count(market)
        .map_err(|e| QuotesError::Network(format!("TDX security_count 失败：{e}")))?;
    let mut start = 0u16;

    loop {
        let rows = client
            .security_list(market, start)
            .map_err(|e| QuotesError::Network(format!("TDX security_list 失败：{e}")))?;
        if rows.is_empty() {
            break;
        }
        let got = rows.len();
        for row in rows {
            classify_row(market, row, out);
        }
        start = start.saturating_add(got as u16);
        if got == 0 || start >= count {
            break;
        }
    }
    Ok(())
}

fn classify_row(market: Market, row: SecurityListEntry, out: &mut TdxUniverse) {
    if row.code.len() != 6 || !row.code.bytes().all(|b| b.is_ascii_digit()) {
        return;
    }
    let name = clean_name(&row.name);
    if name.is_empty() {
        return;
    }

    match market {
        Market::SH => {
            if is_sh_stock(&row.code) {
                out.stocks.push(TdxStockRef {
                    code: row.code,
                    name,
                    market: "sh".into(),
                });
            } else if is_sh_index(&row.code) {
                out.indexes.push(TdxIndexRef {
                    ts_code: format!("{}.SH", row.code),
                    code: row.code,
                    name,
                    market: "SSE".into(),
                });
            } else if let Some(fund_type) = sh_fund_type(&row.code) {
                out.funds.push(TdxFundRef {
                    ts_code: format!("{}.SH", row.code),
                    code: row.code,
                    name,
                    fund_type: Some(fund_type.into()),
                });
            }
        }
        Market::SZ => {
            if is_sz_stock(&row.code) {
                out.stocks.push(TdxStockRef {
                    code: row.code,
                    name,
                    market: "sz".into(),
                });
            } else if is_sz_index(&row.code) {
                out.indexes.push(TdxIndexRef {
                    ts_code: format!("{}.SZ", row.code),
                    code: row.code,
                    name,
                    market: "SZSE".into(),
                });
            } else if let Some(fund_type) = sz_fund_type(&row.code) {
                out.funds.push(TdxFundRef {
                    ts_code: format!("{}.SZ", row.code),
                    code: row.code,
                    name,
                    fund_type: Some(fund_type.into()),
                });
            }
        }
    }
}

fn clean_name(name: &str) -> String {
    let s = name.trim().trim_end_matches('\0').trim().to_string();
    if s.is_empty() || s.contains('\u{fffd}') {
        String::new()
    } else {
        s
    }
}

fn is_sh_stock(code: &str) -> bool {
    starts_with_any(code, &["600", "601", "603", "605", "688"])
}

fn is_sz_stock(code: &str) -> bool {
    starts_with_any(code, &["000", "001", "002", "003", "300", "301"])
}

fn is_sh_index(code: &str) -> bool {
    code.starts_with("000")
}

fn is_sz_index(code: &str) -> bool {
    code.starts_with("399")
}

fn sh_fund_type(code: &str) -> Option<&'static str> {
    if starts_with_any(
        code,
        &[
            "510", "511", "512", "513", "515", "516", "517", "518", "520", "521", "522", "523",
            "524", "525", "526", "560", "561", "562", "563", "588", "589",
        ],
    ) {
        Some("TDX场内基金")
    } else {
        None
    }
}

fn sz_fund_type(code: &str) -> Option<&'static str> {
    if code.starts_with("159") {
        Some("ETF")
    } else if starts_with_any(
        code,
        &[
            "160", "161", "162", "163", "164", "165", "166", "167", "168", "169",
        ],
    ) {
        Some("LOF")
    } else if code.starts_with("184") {
        Some("封闭基金")
    } else {
        None
    }
}

fn starts_with_any(code: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|p| code.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_high_confidence_prefixes() {
        assert!(is_sh_stock("600519"));
        assert!(is_sh_stock("688981"));
        assert!(is_sz_stock("000001"));
        assert!(is_sz_stock("301630"));
        assert!(is_sh_index("000001"));
        assert!(is_sz_index("399006"));
        assert_eq!(sh_fund_type("510300"), Some("TDX场内基金"));
        assert_eq!(sz_fund_type("159915"), Some("ETF"));
        assert_eq!(sz_fund_type("161725"), Some("LOF"));
        assert!(!is_sz_stock("399001"));
        assert_eq!(sh_fund_type("600000"), None);
    }

    #[test]
    fn drops_replacement_char_names() {
        assert_eq!(clean_name("  平安银行\0 "), "平安银行");
        assert_eq!(clean_name("养殖ETF�"), "");
    }
}
