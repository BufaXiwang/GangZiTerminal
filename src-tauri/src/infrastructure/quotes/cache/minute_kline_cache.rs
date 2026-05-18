//! 分钟 K 缓存层——前端 KlineChart 选 1m/5m/15m/60m 时走这里。
//!
//! 与日 K cache 的差别：
//! - TTL 短：盘中 30s（每分钟新增一根），盘外 5min
//! - 时间维度是 unix ms（分钟级），不是日期字符串
//! - 全部走 EM push2his klt 端点（TuShare 分钟 K 5000+ 积分门槛）
//! - 不做复权（分钟级数据复权不实用）

use crate::domain::quotes::{is_a_share_trading_hours, MinutePeriod};
use crate::domain::shared::TsCode;
use crate::infrastructure::db::{migrate, open_database};
use crate::infrastructure::quotes::eastmoney::kline as em_kline;
use rusqlite::{params, OptionalExtension};
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
    find_minute_klines(app, ts_code, period_str(period), limit).unwrap_or_default()
}

/// meta 新鲜检查——返 None 表示需要 ensure。
pub fn check_meta(
    app: &AppHandle,
    ts_code: &str,
    period: MinutePeriod,
) -> Option<MinuteKlineMetaRow> {
    let meta = get_minute_kline_meta(app, ts_code, period_str(period))
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
    upsert_minute_klines(app, &rows)?;
    let last_ts = rows.last().map(|r| r.timestamp_ms).unwrap_or(0);
    upsert_minute_kline_meta(
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

// ============================================================================
// SQLite layer——MinuteKlineRow / MinuteKlineMetaRow + CRUD
// ============================================================================

// ===== 分钟 K 缓存 ==========================================================

pub struct MinuteKlineRow {
    pub ts_code: String,
    pub period: String, // 1m / 5m / 15m / 30m / 60m
    pub timestamp_ms: i64,
    pub open: f64,
    pub close: f64,
    pub high: f64,
    pub low: f64,
    pub volume: i64,
    pub amount: f64,
    pub source: String,
}

#[allow(dead_code)] // get_minute_kline_meta 返回结构，调用方按需读字段
pub struct MinuteKlineMetaRow {
    pub last_known_ts: i64,
    pub fetched_at: String,
}

pub fn upsert_minute_klines(app: &AppHandle, rows: &[MinuteKlineRow]) -> Result<usize, String> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut connection = open_database(app)?;
    migrate(&connection)?;
    let count = rows.len();
    let tx = connection
        .transaction()
        .map_err(|err| format!("开启 minute_klines 事务失败：{err}"))?;
    {
        let mut stmt = tx
            .prepare(
                "insert into minute_klines
                   (ts_code, period, timestamp_ms, open, close, high, low, volume, amount, source)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 on conflict(ts_code, period, timestamp_ms) do update set
                     open = excluded.open,
                     close = excluded.close,
                     high = excluded.high,
                     low = excluded.low,
                     volume = excluded.volume,
                     amount = excluded.amount,
                     source = excluded.source",
            )
            .map_err(|err| format!("准备 minute_klines upsert 失败：{err}"))?;
        for row in rows {
            stmt.execute(params![
                row.ts_code,
                row.period,
                row.timestamp_ms,
                row.open,
                row.close,
                row.high,
                row.low,
                row.volume,
                row.amount,
                row.source,
            ])
            .map_err(|err| {
                format!(
                    "写 minute_kline {}:{}:{} 失败：{err}",
                    row.ts_code, row.period, row.timestamp_ms
                )
            })?;
        }
    }
    tx.commit()
        .map_err(|err| format!("提交 minute_klines 事务失败：{err}"))?;
    Ok(count)
}

/// 拿最近 N 根分钟 K（按 timestamp 升序返回）。
pub fn find_minute_klines(
    app: &AppHandle,
    ts_code: &str,
    period: &str,
    limit: usize,
) -> Result<Vec<MinuteKlineRow>, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    let mut stmt = connection
        .prepare(
            "select ts_code, period, timestamp_ms, open, close, high, low, volume, amount, source
             from (
                 select * from minute_klines
                 where ts_code = ?1 and period = ?2
                 order by timestamp_ms desc
                 limit ?3
             )
             order by timestamp_ms asc",
        )
        .map_err(|err| format!("准备 minute_klines 查询失败：{err}"))?;
    let rows: Vec<MinuteKlineRow> = stmt
        .query_map(params![ts_code, period, limit as i64], |row| {
            Ok(MinuteKlineRow {
                ts_code: row.get(0)?,
                period: row.get(1)?,
                timestamp_ms: row.get(2)?,
                open: row.get(3)?,
                close: row.get(4)?,
                high: row.get(5)?,
                low: row.get(6)?,
                volume: row.get(7)?,
                amount: row.get(8)?,
                source: row.get(9)?,
            })
        })
        .map_err(|err| format!("minute_klines 查询失败：{err}"))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn get_minute_kline_meta(
    app: &AppHandle,
    ts_code: &str,
    period: &str,
) -> Result<Option<MinuteKlineMetaRow>, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    connection
        .query_row(
            "select last_known_ts, fetched_at from minute_kline_meta
             where ts_code = ?1 and period = ?2",
            params![ts_code, period],
            |row| {
                Ok(MinuteKlineMetaRow {
                    last_known_ts: row.get(0)?,
                    fetched_at: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(|err| format!("minute_kline_meta 查询失败：{err}"))
}

pub fn upsert_minute_kline_meta(
    app: &AppHandle,
    ts_code: &str,
    period: &str,
    last_known_ts: i64,
    fetched_at: &str,
) -> Result<(), String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    connection
        .execute(
            "insert into minute_kline_meta (ts_code, period, last_known_ts, fetched_at)
             values (?1, ?2, ?3, ?4)
             on conflict(ts_code, period) do update set
                 last_known_ts = excluded.last_known_ts,
                 fetched_at = excluded.fetched_at",
            params![ts_code, period, last_known_ts, fetched_at],
        )
        .map_err(|err| format!("minute_kline_meta upsert 失败：{err}"))?;
    Ok(())
}
