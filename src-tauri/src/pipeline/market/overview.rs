//! 大盘总览用例——拼装 4 大指数 → MarketOverview。
//!
//! 返 `domain::quotes::MarketOverview`，**不再做 legacy DTO 转换**。
//! caller（adapter / pipeline 内）按需自己转。
//!
//! 当前简化：只拿四大指数，breadth + sectors 留空（待行业涨幅接通后补）。

use crate::domain::quotes::{MarketBreadth, MarketIndex, MarketOverview};
use crate::domain::shared::OccurredAt;
use crate::infrastructure::quotes::tushare::index as ts_index;
use tauri::AppHandle;

const MAIN_INDICES: &[(&str, &str)] = &[
    ("000001.SH", "上证指数"),
    ("399001.SZ", "深证成指"),
    ("399006.SZ", "创业板指"),
    ("000688.SH", "科创50"),
];

/// 拉 4 大指数 + 拼 MarketOverview（domain 富类型）。
pub async fn fetch_market_overview(app: &AppHandle) -> Result<MarketOverview, String> {
    let mut indices: Vec<MarketIndex> = Vec::with_capacity(MAIN_INDICES.len());
    for (ts_code, name) in MAIN_INDICES {
        match ts_index::fetch_index_latest(app, ts_code, name).await {
            Ok(idx) => indices.push(idx),
            Err(e) => tracing::warn!(ts_code, err = %e, "拉指数最新值失败，跳过"),
        }
    }
    if indices.is_empty() {
        return Err("四大指数全部拉取失败".into());
    }
    Ok(MarketOverview {
        indices,
        breadth: MarketBreadth::empty(),
        timestamp: OccurredAt::now(),
    })
}
