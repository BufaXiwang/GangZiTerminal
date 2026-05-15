//! K 线预热——把 Account subscriptions 的日/周/月 K 提前拉到本地。
//!
//! 时机：
//! - 启动后 ~30s 跑一次（让 stocks/indexes/funds 档案先就位）
//! - 每天盘后 16:00 再跑一次（确保当天数据已落 TuShare）
//!
//! 单只标的拉 3 个周期（日/周/月），通过 kline_cache::ensure_klines 增量补全：
//! - 表里没数据：全量拉
//! - 表里有：从 last_known_date+1 增量拉

use crate::domain::quotes::KlinePeriod;
use crate::infrastructure::account::{watchlist, PositionRepo};
use crate::infrastructure::quotes::cache::kline_cache::{self, Category};
use std::collections::BTreeSet;
use std::time::Duration;
use tauri::AppHandle;

const WARM_PERIODS: &[KlinePeriod] = &[KlinePeriod::Day, KlinePeriod::Week, KlinePeriod::Month];
const WARM_LIMIT: usize = 500;
const WARM_INTERVAL_MS: u64 = 120; // 单只标的 × 3 周期 之间的间隔，避免 TuShare 限流

/// 收集需要预热的 ts_code 集合：Account watchlist + open positions。
/// 6 位 code → ts_code 通过 stocks 表 lookup（TuShare 权威 market 字段），**不前缀猜测**。
fn collect_warm_targets(app: &AppHandle) -> Vec<String> {
    let watchlist_codes: Vec<String> = watchlist::list()
        .into_iter()
        .map(|c| c.as_str().to_string())
        .collect();
    let pos_codes: Vec<String> = PositionRepo::new(app.clone())
        .list_open()
        .map(|ps| {
            ps.into_iter()
                .map(|p| p.code.as_str().to_string())
                .collect()
        })
        .unwrap_or_default();
    tracing::info!(
        watchlist = watchlist_codes.len(),
        positions_open = pos_codes.len(),
        "kline_warm collect_warm_targets"
    );
    let mut codes: BTreeSet<String> = watchlist_codes.into_iter().collect();
    for c in pos_codes {
        codes.insert(c);
    }
    let mut targets = Vec::with_capacity(codes.len());
    let mut unresolved = 0usize;
    for code in codes {
        match crate::db::resolve_stock_ts_code(app, &code) {
            Some(ts) => targets.push(ts),
            None => unresolved += 1,
        }
    }
    if unresolved > 0 {
        tracing::warn!(
            unresolved,
            "kline_warm 有 {unresolved} 个 code 在 stocks 表未命中，跳过预热"
        );
    }
    targets
}

/// 跑一次预热——串行 ensure 每个标的 × 3 周期。
pub async fn warm_klines_once(app: &AppHandle) {
    let targets = collect_warm_targets(app);
    if targets.is_empty() {
        tracing::debug!("K 线预热：Account subscriptions 为空，跳过");
        return;
    }
    tracing::info!(count = targets.len(), "K 线预热开始");

    let mut total_calls = 0usize;
    let mut failed = 0usize;
    for ts_code in &targets {
        for period in WARM_PERIODS {
            match kline_cache::ensure_klines(
                app,
                ts_code,
                Category::Stock,
                *period,
                Category::Stock.default_adj(),
                WARM_LIMIT,
            )
            .await
            {
                Ok(()) => {}
                Err(e) => {
                    failed += 1;
                    tracing::debug!(ts_code, ?period, err = %e, "预热单只失败");
                }
            }
            total_calls += 1;
            tokio::time::sleep(Duration::from_millis(WARM_INTERVAL_MS)).await;
        }
    }
    tracing::info!(
        targets = targets.len(),
        calls = total_calls,
        failed,
        "K 线预热完成"
    );
}
