//! A 股股票档案刷新：name / code / sector / market 本地映射的写入用例。
//!
//! 数据源：
//! - TuShare `stock_basic`：权威档案源（含主板 / 科创板 / 创业板 / 北交所）
//! - TDX universe：fallback minimal source（无 sector，但 9000+ 主板覆盖）
//!
//! 刷新策略：
//! - 启动时若 stocks/indexes/funds 任一表为空 → 立刻拉一次冷启动数据
//! - 每天 08:30 北京时间盘前预热（覆盖新股 / 改名 / 摘牌）
//! - 失败仅日志告警，不影响其它流水线

use crate::infrastructure::quotes::repository::{FundRow, IndexRow, StockRow};
use crate::infrastructure::quotes::tdx::universe::TdxUniverse;
use crate::infrastructure::quotes::tushare::stock as ts_stock;
use serde_json::json;
use tauri::{AppHandle, Emitter};

/// 从 TuShare `stock_basic` 拉全市场 A 股档案，写入 `stocks` 表。返回写入条数。
///
/// TuShare 是权威档案源；失败时由 `refresh_universe` 统一走 TDX minimal fallback。
///
/// 失败时不改动现有表（事务回滚）；caller 决定要不要重试 / 等下次定时刷新。
pub async fn refresh_now(app: &AppHandle) -> Result<usize, String> {
    let refs = ts_stock::fetch_all_stocks(app)
        .await
        .map_err(|e| e.to_string())?;
    let rows: Vec<StockRow> = refs
        .into_iter()
        .map(|s| StockRow {
            code: s.code.as_str().to_string(),
            name: s.name,
            sector: s.sector,
            market: s.market,
        })
        .collect();
    crate::infrastructure::quotes::repository::upsert_stocks(app.clone(), rows)
}

/// 拉指数档案（SSE / SZSE / CSI 合并）写入 `indexes` 表。
///
/// 失败仅 warn，不向上传——指数档案缺失只影响今日市场列表的"指数"tab 内容，
/// 不影响 Account watchlist 订阅和模拟交易。
pub async fn refresh_indexes(app: &AppHandle) -> Result<usize, String> {
    let payload = crate::infrastructure::quotes::tushare::index::fetch_all_common_indexes(app)
        .await
        .map_err(|e| e.to_string())?;
    let rows: Vec<crate::infrastructure::quotes::repository::IndexRow> = payload
        .into_iter()
        .map(|b| crate::infrastructure::quotes::repository::IndexRow {
            ts_code: b.ts_code,
            code: b.code,
            name: b.name,
            market: b.market,
            publisher: b.publisher,
            category: b.category,
        })
        .collect();
    crate::infrastructure::quotes::repository::upsert_indexes(app.clone(), rows)
}

/// 拉场内基金档案（ETF / LOF / 封基）写入 `funds` 表。
pub async fn refresh_funds(app: &AppHandle) -> Result<usize, String> {
    let payload = crate::infrastructure::quotes::tushare::fund::fetch_listed_funds(app)
        .await
        .map_err(|e| e.to_string())?;
    let rows: Vec<crate::infrastructure::quotes::repository::FundRow> = payload
        .into_iter()
        .map(|b| crate::infrastructure::quotes::repository::FundRow {
            ts_code: b.ts_code,
            code: b.code,
            name: b.name,
            market: b.market,
            fund_type: b.fund_type,
            management: b.management,
            list_date: b.list_date,
            status: b.status,
        })
        .collect();
    crate::infrastructure::quotes::repository::upsert_funds(app.clone(), rows)
}

fn write_tdx_stocks(app: &AppHandle, universe: &TdxUniverse) -> Result<usize, String> {
    let rows: Vec<StockRow> = universe
        .stocks
        .iter()
        .map(|s| StockRow {
            code: s.code.clone(),
            name: s.name.clone(),
            sector: None,
            market: s.market.clone(),
        })
        .collect();
    crate::infrastructure::quotes::repository::upsert_stocks_minimal(app.clone(), rows)
}

fn write_tdx_indexes(app: &AppHandle, universe: &TdxUniverse) -> Result<usize, String> {
    let rows: Vec<IndexRow> = universe
        .indexes
        .iter()
        .map(|i| IndexRow {
            ts_code: i.ts_code.clone(),
            code: i.code.clone(),
            name: i.name.clone(),
            market: i.market.clone(),
            publisher: Some("TDX".into()),
            category: None,
        })
        .collect();
    crate::infrastructure::quotes::repository::upsert_indexes_minimal(app.clone(), rows)
}

fn write_tdx_funds(app: &AppHandle, universe: &TdxUniverse) -> Result<usize, String> {
    let rows: Vec<FundRow> = universe
        .funds
        .iter()
        .map(|f| FundRow {
            ts_code: f.ts_code.clone(),
            code: f.code.clone(),
            name: f.name.clone(),
            market: "E".into(),
            fund_type: f.fund_type.clone(),
            management: None,
            list_date: None,
            status: Some("L".into()),
        })
        .collect();
    crate::infrastructure::quotes::repository::upsert_funds_minimal(app.clone(), rows)
}

/// 全市场档案三件套刷新——stocks + indexes + funds。
/// TuShare 是权威源；任一 TuShare 档案失败时，一次性拉 TDX security_list 作为
/// SH/SZ 的最小档案 fallback。TDX minimal upsert 不会清空 TuShare 已有元数据。
pub async fn refresh_universe(app: &AppHandle) -> (usize, usize, usize) {
    let stocks_result = refresh_now(app).await;
    let indexes_result = refresh_indexes(app).await;
    let funds_result = refresh_funds(app).await;

    let needs_tdx = stocks_result.is_err() || indexes_result.is_err() || funds_result.is_err();
    let tdx_universe = if needs_tdx {
        match crate::infrastructure::quotes::tdx::universe::fetch_universe().await {
            Ok(u) => Some(u),
            Err(e) => {
                tracing::warn!(error = %e, "TDX minimal 档案 fallback 失败");
                None
            }
        }
    } else {
        None
    };

    let stocks = match stocks_result {
        Ok(n) => {
            tracing::info!(count = n, source = "tushare", "stocks 档案刷新成功");
            n
        }
        Err(e) => {
            tracing::warn!(error = %e, "stocks TuShare 档案刷新失败，尝试 TDX minimal fallback");
            match tdx_universe.as_ref().map(|u| write_tdx_stocks(app, u)) {
                Some(Ok(n)) => {
                    tracing::info!(count = n, source = "tdx", "stocks minimal 档案刷新成功");
                    n
                }
                Some(Err(err)) => {
                    tracing::warn!(error = %err, "stocks TDX minimal 档案写入失败");
                    0
                }
                None => 0,
            }
        }
    };
    let indexes = match indexes_result {
        Ok(n) => {
            tracing::info!(count = n, source = "tushare", "indexes 档案刷新成功");
            n
        }
        Err(e) => {
            tracing::warn!(error = %e, "indexes TuShare 档案刷新失败，尝试 TDX minimal fallback");
            match tdx_universe.as_ref().map(|u| write_tdx_indexes(app, u)) {
                Some(Ok(n)) => {
                    tracing::info!(count = n, source = "tdx", "indexes minimal 档案刷新成功");
                    n
                }
                Some(Err(err)) => {
                    tracing::warn!(error = %err, "indexes TDX minimal 档案写入失败");
                    0
                }
                None => 0,
            }
        }
    };
    let funds = match funds_result {
        Ok(n) => {
            tracing::info!(count = n, source = "tushare", "funds 档案刷新成功");
            n
        }
        Err(e) => {
            tracing::warn!(error = %e, "funds TuShare 档案刷新失败，尝试 TDX minimal fallback");
            match tdx_universe.as_ref().map(|u| write_tdx_funds(app, u)) {
                Some(Ok(n)) => {
                    tracing::info!(count = n, source = "tdx", "funds minimal 档案刷新成功");
                    n
                }
                Some(Err(err)) => {
                    tracing::warn!(error = %err, "funds TDX minimal 档案写入失败");
                    0
                }
                None => 0,
            }
        }
    };

    // 通知前端档案表已更新 —— useMarketInstruments listen 这个事件后 re-invoke list_market_instruments
    let _ = app.emit(
        "market-instruments-refreshed",
        json!({
            "stocks": stocks,
            "indexes": indexes,
            "funds": funds,
            "refreshedAt": chrono::Utc::now().to_rfc3339(),
        }),
    );

    (stocks, indexes, funds)
}

/// 保存 TuShare token + 立刻拉一次全市场档案。
///
/// 走这条命令而不是通用的 `save_app_state` 是因为：scheduler 里的几个 loop
/// 只在 backend 启动后短窗口内做冷启动检查（stocks_refresh_loop 启动+3s、
/// tushare_probe_once 启动+20s）。用户在 Settings 里填完 token 时这些窗口早
/// 过了，下一次刷新要等北京 08:30——所以需要在保存 token 的当下主动 spawn
/// 一次 refresh_universe，让 stocks/indexes/funds 三表立刻填上。
///
/// 同时删 probe-done flag，让下次 backend 重启时能重新跑一遍 TuShare 能力探测
/// （旧 flag 是用旧 token 跑的，结果不可信）。
pub async fn save_tushare_token(app: AppHandle, token: String) -> Result<(), String> {
    let trimmed = token.trim().to_string();
    crate::infrastructure::app_state::save_app_state_value(
        &app,
        "gangzi-terminal.tushare-token", // 与 infrastructure/quotes/tushare/client.rs KEY_TUSHARE_TOKEN 一致
        &serde_json::Value::String(trimmed.clone()),
    )?;
    // 老 probe 结果用的是旧 token —— 删 flag 让下次 backend 启动时重新探测
    crate::infrastructure::app_state::delete_app_state_value(
        &app,
        "gangzi-terminal.tushare-probe-done",
    )?;

    // token 为空（用户清空）→ 不 spawn refresh，避免对空 token 跑 68 个失败请求
    if !trimmed.is_empty() {
        let app_for_spawn = app.clone();
        tauri::async_runtime::spawn(async move {
            tracing::info!("token 已更新，立刻拉一次全市场档案");
            let (s, i, f) = refresh_universe(&app_for_spawn).await;
            tracing::info!(
                stocks = s,
                indexes = i,
                funds = f,
                "token 更新后档案刷新完成"
            );
        });
    }
    Ok(())
}

