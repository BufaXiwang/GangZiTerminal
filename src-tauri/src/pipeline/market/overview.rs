//! 大盘总览用例——拼装 4 大指数 → MarketOverview。
//!
//! 返 `domain::quotes::MarketOverview`，**不再做 legacy DTO 转换**。
//! caller（adapter / pipeline 内）按需自己转。
//!
//! 当前简化：breadth 已从 market_snapshot 派生；sectors 暂留空（接通 ths_index/
//! index_dailybasic 后再补——daily 全市场聚合开销大，不适合放在用户每次进首页调一次的
//! overview 里，需要先加日级 cache 才能 hot path 使用）。

use crate::domain::quotes::{MarketBreadth, MarketIndex, MarketOverview};
use crate::domain::shared::OccurredAt;
use crate::infrastructure::quotes::snapshot::market_snapshot;
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
    // Breadth：直接从 in-memory snapshot 派生——market_quote_loop 每 15s 维护订阅集，
    // market_universe_loop 每 60s/5min 维护全市场约 5800 只票。snapshot 大小不够大时
    // 数字会偏小，前端可显示"snapshot=N，覆盖度不全"。
    let (rise, fall, flat) = market_snapshot::compute_breadth();
    let breadth = if rise == 0 && fall == 0 && flat == 0 {
        // 启动早期 snapshot 还没 hydrate——降级到 empty，前端"--"展示。
        MarketBreadth::empty()
    } else {
        MarketBreadth { rise, fall, flat }
    };

    Ok(MarketOverview {
        indices,
        breadth,
        timestamp: OccurredAt::now(),
    })
}
