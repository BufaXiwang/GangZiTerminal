//! 分钟 K 缓存层——前端 KlineChart 选 1m/5m/15m/60m 时走这里。
//!
//! 与日 K cache 的差别：
//! - TTL 短：盘中 30s（每分钟新增一根），盘外 5min
//! - 时间维度是 unix ms（分钟级），不是日期字符串
//! - 全部走 EM push2his klt 端点（TuShare 分钟 K 5000+ 积分门槛）
//! - 不做复权（分钟级数据复权不实用）

use crate::db::{self, MinuteKlineMetaRow, MinuteKlineRow};
use crate::domain::quotes::{is_a_share_trading_hours, MinutePeriod};
use crate::domain::shared::TsCode;
use crate::infrastructure::quotes::eastmoney::kline as em_kline;
use tauri::AppHandle;

/// 默认抓取根数——足够画 4 小时分时（240 个 1m）
const DEFAULT_LIMIT: usize = 800;

fn ttl_secs() -> i64 {
    if is_a_share_trading_hours() {
        30
    } else {
        300
    }
}

fn period_str(p: MinutePeriod) -> &'static str {
    match p {
        MinutePeriod::M1 => "1m",
        MinutePeriod::M5 => "5m",
        MinutePeriod::M15 => "15m",
        MinutePeriod::M30 => "30m",
        MinutePeriod::M60 => "60m",
    }
}

pub fn parse_period(s: &str) -> Option<MinutePeriod> {
    match s {
        "1m" => Some(MinutePeriod::M1),
        "5m" => Some(MinutePeriod::M5),
        "15m" => Some(MinutePeriod::M15),
        "30m" => Some(MinutePeriod::M30),
        "60m" => Some(MinutePeriod::M60),
        _ => None,
    }
}

/// 同步读——从 SQLite 拿最近 limit 根分钟 K（升序 旧→新）。不触发刷新。
pub fn read(
    app: &AppHandle,
    ts_code: &str,
    period: MinutePeriod,
    limit: usize,
) -> Vec<MinuteKlineRow> {
    db::find_minute_klines(app, ts_code, period_str(period), limit).unwrap_or_default()
}

/// meta 新鲜检查——返 None 表示需要 ensure。
pub fn check_meta(
    app: &AppHandle,
    ts_code: &str,
    period: MinutePeriod,
) -> Option<MinuteKlineMetaRow> {
    let meta = db::get_minute_kline_meta(app, ts_code, period_str(period))
        .ok()
        .flatten()?;
    if parse_age_secs(&meta.fetched_at) < ttl_secs() {
        Some(meta)
    } else {
        None
    }
}

/// 拉远端 → upsert DB → 更新 meta。
pub async fn ensure(app: &AppHandle, ts_code: &str, period: MinutePeriod) -> Result<(), String> {
    let parsed_ts = TsCode::new(ts_code).map_err(|e| e.to_string())?;
    let series = em_kline::fetch_minute_klines(&parsed_ts, period, DEFAULT_LIMIT)
        .await
        .map_err(|e| e.to_string())?;
    if series.points.is_empty() {
        return Ok(());
    }
    let rows: Vec<MinuteKlineRow> = series
        .points
        .iter()
        .map(|p| MinuteKlineRow {
            ts_code: ts_code.to_string(),
            period: period_str(period).to_string(),
            timestamp_ms: p.timestamp.value(),
            open: p.open.value(),
            close: p.close.value(),
            high: p.high.value(),
            low: p.low.value(),
            volume: p.volume.value(),
            amount: p.amount.value(),
            source: "em".to_string(),
        })
        .collect();
    db::upsert_minute_klines(app, &rows)?;
    let last_ts = rows.last().map(|r| r.timestamp_ms).unwrap_or(0);
    db::upsert_minute_kline_meta(
        app,
        ts_code,
        period_str(period),
        last_ts,
        &chrono::Utc::now().to_rfc3339(),
    )?;
    Ok(())
}

/// 一次性 API——adapter / agent 直接调：TTL 命中读 DB，否则阻塞 ensure 后读。
pub async fn get_or_refresh(
    app: &AppHandle,
    ts_code: &str,
    period: MinutePeriod,
    limit: usize,
) -> Result<Vec<MinuteKlineRow>, String> {
    if check_meta(app, ts_code, period).is_some() {
        let rows = read(app, ts_code, period, limit);
        if !rows.is_empty() {
            return Ok(rows);
        }
    }
    if let Err(e) = ensure(app, ts_code, period).await {
        tracing::warn!(ts_code, ?period, err = %e, "ensure_minute_klines 失败，回退 stale");
    }
    Ok(read(app, ts_code, period, limit))
}

fn parse_age_secs(rfc3339: &str) -> i64 {
    match chrono::DateTime::parse_from_rfc3339(rfc3339) {
        Ok(t) => (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds(),
        Err(_) => i64::MAX,
    }
}
