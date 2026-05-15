//! K 线缓存层——前端读 / 增量刷新的统一封装。
//!
//! 设计：
//! - 持久化层：SQLite `klines` + `kline_meta`（ts_code PK）
//! - 增量刷新：根据 `kline_meta.last_known_date` 决定区间拉
//! - TTL：10 分钟内可用 → 直接读 DB；否则触发 ensure
//! - 三类分流：stock → TuShare daily；index → index_daily；fund → fund_daily
//!
//! 调用方：
//! - adapter `fetch_a_share_klines`（前端 KlineChart）
//! - pipeline `kline_warm`（启动后预热 Account subscriptions）

use crate::db::{self, KlineMetaRow, KlineRow};
use crate::domain::quotes::{AdjMode, KlinePeriod, KlineSeries};
use crate::domain::shared::StockCode;
use crate::infrastructure::quotes::tushare::{
    fund as ts_fund, index as ts_index, stock as ts_stock,
};
use tauri::AppHandle;

/// 缓存层 TTL——meta.fetched_at 距今 > 10 分钟 → 需要刷新
const META_TTL_SECS: i64 = 10 * 60;

/// 标的类别——驱动调用哪个 TuShare 接口
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Stock,
    Index,
    Fund,
}

impl Category {
    pub fn from_str(s: &str) -> Self {
        match s {
            "index" => Self::Index,
            "fund" => Self::Fund,
            _ => Self::Stock,
        }
    }

    /// 默认复权模式——个股 qfq，指数 / 基金 none
    pub fn default_adj(self) -> AdjMode {
        match self {
            Self::Stock => AdjMode::Qfq,
            _ => AdjMode::None,
        }
    }
}

fn adj_str(a: AdjMode) -> &'static str {
    match a {
        AdjMode::Qfq => "qfq",
        AdjMode::Hfq => "hfq",
        AdjMode::None => "none",
    }
}

fn period_str(p: KlinePeriod) -> &'static str {
    match p {
        KlinePeriod::Day => "day",
        KlinePeriod::Week => "week",
        KlinePeriod::Month => "month",
    }
}

/// 同步读——从 SQLite 拿最近 limit 根 K 线（升序 旧→新）。
/// 不触发刷新，调用方负责先 ensure。
pub fn read_klines(
    app: &AppHandle,
    ts_code: &str,
    period: KlinePeriod,
    adj: AdjMode,
    limit: usize,
) -> Vec<KlineRow> {
    db::find_klines(app, ts_code, period_str(period), adj_str(adj), limit).unwrap_or_default()
}

/// 检查 meta 是否新鲜——返 None 表示需要 ensure。
pub fn check_meta(
    app: &AppHandle,
    ts_code: &str,
    period: KlinePeriod,
    adj: AdjMode,
) -> Option<KlineMetaRow> {
    let meta = db::get_kline_meta(app, ts_code, period_str(period), adj_str(adj))
        .ok()
        .flatten()?;
    let age = parse_age_secs(&meta.fetched_at);
    if age < META_TTL_SECS {
        Some(meta)
    } else {
        None
    }
}

/// 确保 K 线缓存最新——按 meta 决定全量 / 增量拉。完成后写 DB。
///
/// 调用语义：阻塞等到数据落库再返。前端 adapter 在 cache miss / stale 时调这个。
pub async fn ensure_klines(
    app: &AppHandle,
    ts_code: &str,
    category: Category,
    period: KlinePeriod,
    adj: AdjMode,
    limit: usize,
) -> Result<(), String> {
    // 1. 查 meta，决定拉法
    let meta = db::get_kline_meta(app, ts_code, period_str(period), adj_str(adj))
        .ok()
        .flatten();
    let start_date: Option<String> = meta.as_ref().and_then(|m| {
        // 增量起点 = last_known_date + 1 个交易日（用日历日 +1 也行，TuShare 跳过非交易日）
        next_compact_date(&m.last_known_date).ok()
    });

    // 2. 拉外部
    let series = fetch_from_remote(
        app,
        ts_code,
        category,
        period,
        adj,
        limit,
        start_date.as_deref(),
    )
    .await
    .map_err(|e| format!("拉外部 K 线失败：{e}"))?;

    if series.points.is_empty() {
        // 没新数据（增量场景常见）—— 只更新 meta.fetched_at，不动 klines
        if let Some(m) = meta {
            let _ = db::upsert_kline_meta(
                app,
                ts_code,
                period_str(period),
                adj_str(adj),
                &m.last_known_date,
                &chrono::Utc::now().to_rfc3339(),
            );
        }
        return Ok(());
    }

    // 3. upsert klines
    let rows: Vec<KlineRow> = series
        .points
        .iter()
        .map(|p| KlineRow {
            ts_code: ts_code.to_string(),
            period: period_str(period).to_string(),
            adjust: adj_str(adj).to_string(),
            date: p.date.to_compact(),
            open: p.open.value(),
            close: p.close.value(),
            high: p.high.value(),
            low: p.low.value(),
            volume: Some(p.volume.value() as f64),
            amount: Some(p.amount.value()),
            source: "tushare".to_string(),
        })
        .collect();
    db::upsert_klines(app, &rows)?;

    // 4. 更新 meta
    let last_date = rows.last().map(|r| r.date.clone()).unwrap_or_default();
    let new_last = match meta.as_ref() {
        Some(m) if m.last_known_date > last_date => m.last_known_date.clone(),
        _ => last_date,
    };
    db::upsert_kline_meta(
        app,
        ts_code,
        period_str(period),
        adj_str(adj),
        &new_last,
        &chrono::Utc::now().to_rfc3339(),
    )?;
    Ok(())
}

/// 一次性 API——前端 adapter 直接调：检查 TTL → 必要时 ensure → 读 DB 返。
pub async fn get_or_refresh(
    app: &AppHandle,
    ts_code: &str,
    category: Category,
    period: KlinePeriod,
    adj: AdjMode,
    limit: usize,
) -> Result<Vec<KlineRow>, String> {
    // 1. meta 新鲜 → 直接读
    if check_meta(app, ts_code, period, adj).is_some() {
        let rows = read_klines(app, ts_code, period, adj, limit);
        if !rows.is_empty() {
            return Ok(rows);
        }
        // meta 新鲜但 klines 表为空（异常）→ 强制 ensure
    }

    // 2. ensure 后再读
    if let Err(e) = ensure_klines(app, ts_code, category, period, adj, limit).await {
        tracing::warn!(ts_code, ?period, err = %e, "ensure_klines 失败，尝试读 stale 缓存");
    }
    Ok(read_klines(app, ts_code, period, adj, limit))
}

// ===== 内部 helpers =======================================================

async fn fetch_from_remote(
    app: &AppHandle,
    ts_code: &str,
    category: Category,
    period: KlinePeriod,
    adj: AdjMode,
    limit: usize,
    start_date: Option<&str>,
) -> Result<KlineSeries, String> {
    let end_date = Some(today_compact());
    match category {
        Category::Stock => {
            // ts_code 形如 "600519.SH"，TuShare daily 接口需要 ts_code
            // StockCode 是 6 位——我们直接用 ts_code 拼回 StockCode（市场后缀已知）
            let code = ts_code
                .split('.')
                .next()
                .ok_or_else(|| format!("非法 ts_code: {ts_code}"))?;
            let stock_code = StockCode::new(code).map_err(|e| e.to_string())?;
            ts_stock::fetch_klines_in_range(
                app,
                &stock_code,
                period,
                limit,
                adj,
                start_date,
                end_date.as_deref(),
            )
            .await
            .map_err(|e| e.to_string())
        }
        Category::Index => ts_index::fetch_index_klines_in_range(
            app,
            ts_code,
            period,
            limit,
            start_date,
            end_date.as_deref(),
        )
        .await
        .map_err(|e| e.to_string()),
        Category::Fund => ts_fund::fetch_fund_klines_in_range(
            app,
            ts_code,
            period,
            limit,
            adj,
            start_date,
            end_date.as_deref(),
        )
        .await
        .map_err(|e| e.to_string()),
    }
}

fn parse_age_secs(rfc3339: &str) -> i64 {
    match chrono::DateTime::parse_from_rfc3339(rfc3339) {
        Ok(t) => (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds(),
        Err(_) => i64::MAX,
    }
}

fn today_compact() -> String {
    let beijing = chrono::Utc::now() + chrono::Duration::hours(8);
    beijing.format("%Y%m%d").to_string()
}

fn next_compact_date(compact: &str) -> Result<String, String> {
    // "20250513" → "20250514"
    let date = chrono::NaiveDate::parse_from_str(compact, "%Y%m%d")
        .map_err(|e| format!("非法日期 {compact}：{e}"))?;
    let next = date + chrono::Duration::days(1);
    Ok(next.format("%Y%m%d").to_string())
}
