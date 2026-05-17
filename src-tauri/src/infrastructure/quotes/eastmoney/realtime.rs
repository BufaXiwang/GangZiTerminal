//! EM `ulist.np` 实时报价（**仅基础字段，无五档**）。
//!
//! 字段（fields=）：
//! - f12 code / f13 market / f14 name
//! - f2 price / f3 涨跌幅 / f4 涨跌额
//! - f5 成交量 / f6 成交额
//! - f15 高 / f16 低 / f17 开 / f18 昨收
//! - f124 行情时间戳（秒级，× 1000 转毫秒）
//!
//! 不带五档（f19-f40 / f29-f30 / f191-f192）——EM 对长 URL + 含五档请求触发
//! Empty reply 频率明显高于精简字段；五档需求由 account 模块（持仓平仓）或
//! 前端详情视图按需 lazy 拉。

use super::client::{fetch_text, fetch_text_with, parse_em_response};
use crate::domain::quotes::{QuotesError, StockQuote};
use crate::domain::shared::{Lots, OccurredAt, StockCode, Yuan};
use serde_json::Value;

const FIELDS: &str = "f12,f13,f14,f2,f3,f4,f5,f6,f15,f16,f17,f18,f124";

/// 批量拉实时报价（基础字段）。
///
/// 给 secids 列表（含市场前缀，如 "1.000001" / "0.159915" / "2.920469"），
/// 返回 `(ts_code, StockQuote)`——ts_code 形如 "{code}.{SH|SZ|BJ}"。
pub async fn fetch_quotes_by_secids(
    secids: &[String],
) -> Result<Vec<(String, StockQuote)>, QuotesError> {
    if secids.is_empty() {
        return Ok(Vec::new());
    }
    let url = build_url(secids);
    let body = fetch_text(&url, "实时报价").await?;
    parse_diff(&body)
}

/// 同 `fetch_quotes_by_secids`，但允许调用方传入自定义 Client（含 proxy）。
/// realtime 多源 dispatch 走这个路径，绑 proxy_pool 借出的 client。
pub async fn fetch_quotes_by_secids_with(
    client: &reqwest::Client,
    secids: &[String],
) -> Result<Vec<(String, StockQuote)>, QuotesError> {
    if secids.is_empty() {
        return Ok(Vec::new());
    }
    let url = build_url(secids);
    let body = fetch_text_with(client, &url, "实时报价").await?;
    parse_diff(&body)
}

fn build_url(secids: &[String]) -> String {
    format!(
        "https://push2.eastmoney.com/api/qt/ulist.np/get?fltt=2&invt=2&fields={}&secids={}",
        FIELDS,
        secids.join(",")
    )
}

fn parse_diff(body: &str) -> Result<Vec<(String, StockQuote)>, QuotesError> {
    let value = parse_em_response(body, "实时报价")?;
    let diff = value
        .pointer("/data/diff")
        .and_then(Value::as_array)
        .ok_or_else(|| QuotesError::Decode("ulist.np 响应缺 data.diff".into()))?;

    // EM 返回不带市场前缀，要从 f13 推断（0=深 / 1=沪 / 2=北）。
    // 极少数特殊代码段（中证 100/101 等）会跳过+ log。
    let mut unknown_markets = std::collections::HashSet::new();
    let result: Vec<(String, StockQuote)> = diff
        .iter()
        .filter_map(|item| {
            let code_str = item.get("f12").and_then(|v| v.as_str())?.to_string();
            let market_num = item.get("f13").and_then(|v| v.as_i64()).unwrap_or(-1);
            let suffix = match market_num {
                0 => "SZ",
                1 => "SH",
                2 => "BJ",
                other => {
                    unknown_markets.insert(other);
                    return None;
                }
            };
            let ts_code = format!("{code_str}.{suffix}");
            let code = StockCode::new(&code_str).ok()?;
            let name = item
                .get("f14")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let quote = StockQuote {
                code,
                name,
                price: num_f64(item, "f2").map(Yuan::from_unchecked),
                change_percent: num_f64(item, "f3"),
                change: num_f64(item, "f4").map(Yuan::from_unchecked),
                day_volume: num_f64(item, "f5").map(|v| Lots::from_unchecked(v as i64)),
                day_amount: num_f64(item, "f6").map(Yuan::from_unchecked),
                high: num_f64(item, "f15").map(Yuan::from_unchecked),
                low: num_f64(item, "f16").map(Yuan::from_unchecked),
                open: num_f64(item, "f17").map(Yuan::from_unchecked),
                previous_close: num_f64(item, "f18").map(Yuan::from_unchecked),
                captured_at: OccurredAt::now(),
                bid_levels: Vec::new(),
                ask_levels: Vec::new(),
                buy_volume: None,
                sell_volume: None,
                order_imbalance: None,
            };
            Some((ts_code, quote))
        })
        .collect();

    if result.is_empty() && !diff.is_empty() {
        let head: Vec<String> = diff.iter().take(3).map(|v| v.to_string()).collect();
        tracing::warn!(
            unknown_markets = ?unknown_markets,
            sample = ?head,
            "EM ulist.np diff 非空但全部跳过——可能 f13 市场号 unknown"
        );
    } else if !unknown_markets.is_empty() {
        tracing::debug!(
            unknown_markets = ?unknown_markets,
            kept = result.len(),
            "EM ulist.np 部分 item 因未知 market 跳过"
        );
    }

    Ok(result)
}

fn num_f64(item: &Value, key: &str) -> Option<f64> {
    let v = item.get(key)?;
    if let Some(n) = v.as_f64() {
        if n.is_finite() && n > -1e15 {
            return Some(n);
        }
    }
    v.as_str().and_then(|s| s.parse::<f64>().ok())
}
