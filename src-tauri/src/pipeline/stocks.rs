//! A 股股票档案：name / code / sector 的本地映射。
//!
//! 为什么要这张表：
//! - Agent 工具需要支持"按名字查行情"（"茅台"、"贵州茅台" 都行），不能强制 6 位代码。
//! - 名字 → 代码的关系基本不变（新股+退市 ≤ 10/天），不需要高频拉，但要在本地查得快。
//!
//! 刷新策略：
//! - 启动时若 `stocks` 表为空 → 立刻拉一次（冷启动兜底，让 resolve_stock 第一时间可用）。
//! - 每天 08:30 北京时间 → 盘前预热拉一次（覆盖前一晚的新股 / 改名 / 摘牌）。
//! - 失败仅日志告警，不影响其它流水线。

use crate::infrastructure::quotes::repository::StockRow;
use crate::domain::shared::StockCode;
use crate::infrastructure::quotes::eastmoney::realtime as em_realtime;
use crate::infrastructure::quotes::tushare::stock as ts_stock;
use serde_json::json;
use tauri::{AppHandle, Emitter};

/// 全市场股票档案的查询结果——code / name / sector / market。
#[derive(Debug, Clone)]
#[allow(dead_code)] // agent 工具 Step B 用 name/sector/market 做 prompt 展示与缓存键
pub struct StockRef {
    pub code: String,
    pub name: String,
    pub sector: Option<String>,
    pub market: String,
}

impl From<StockRow> for StockRef {
    fn from(row: StockRow) -> Self {
        Self {
            code: row.code,
            name: row.name,
            sector: row.sector,
            market: row.market,
        }
    }
}

/// 从 TuShare `stock_basic` 拉全市场 A 股档案，写入 `stocks` 表。返回写入条数。
///
/// 数据源：**仅 TuShare**——之前的 EM clist 路径偶发风控不稳，这次重构后
/// stocks 表只走 TuShare。没配 token 时返 `MissingToken` 错误，业务侧应该让
/// 用户去 Settings 配；不再回落 EM。
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

/// 全市场档案三件套刷新——stocks + indexes + funds。
/// 任一失败不影响其它（独立 try）。返回各自写入条数 (stocks, indexes, funds)。
pub async fn refresh_universe(app: &AppHandle) -> (usize, usize, usize) {
    let stocks = match refresh_now(app).await {
        Ok(n) => {
            tracing::info!(count = n, "stocks 档案刷新成功");
            n
        }
        Err(e) => {
            tracing::warn!(error = %e, "stocks 档案刷新失败");
            0
        }
    };
    let indexes = match refresh_indexes(app).await {
        Ok(n) => {
            tracing::info!(count = n, "indexes 档案刷新成功");
            n
        }
        Err(e) => {
            tracing::warn!(error = %e, "indexes 档案刷新失败");
            0
        }
    };
    let funds = match refresh_funds(app).await {
        Ok(n) => {
            tracing::info!(count = n, "funds 档案刷新成功");
            n
        }
        Err(e) => {
            tracing::warn!(error = %e, "funds 档案刷新失败");
            0
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
#[tauri::command]
pub async fn save_tushare_token(app: AppHandle, token: String) -> Result<(), String> {
    let trimmed = token.trim().to_string();
    crate::infrastructure::app_state::save_app_state_value(
        &app,
        "gangzi-terminal.tushare-token", // 与 infrastructure/quotes/tushare/client.rs KEY_TUSHARE_TOKEN 一致
        &serde_json::Value::String(trimmed.clone()),
    )?;
    // 老 probe 结果用的是旧 token —— 删 flag 让下次 backend 启动时重新探测
    crate::infrastructure::app_state::delete_app_state_value(&app, "gangzi-terminal.tushare-probe-done")?;

    // token 为空（用户清空）→ 不 spawn refresh，避免对空 token 跑 68 个失败请求
    if !trimmed.is_empty() {
        let app_for_spawn = app.clone();
        tauri::async_runtime::spawn(async move {
            tracing::info!("token 已更新，立刻拉一次全市场档案");
            let (s, i, f) = refresh_universe(&app_for_spawn).await;
            tracing::info!(stocks = s, indexes = i, funds = f, "token 更新后档案刷新完成");
        });
    }
    Ok(())
}

/// 个股交易所前缀推断：6 沪市、4/8/92 北交所、其它深市。
/// 个股语义——000001 = 平安银行 (sz)，不是上证指数。
fn market_prefix(code: &str) -> &'static str {
    if code.starts_with('6') {
        "sh"
    } else if code.starts_with('4') || code.starts_with('8') || code.starts_with("92") {
        "bj"
    } else {
        "sz"
    }
}

/// 把 agent 输入的"identifier"（6 位代码 / 中文名 / 部分中文名）解析成股票档案。
///
/// 解析策略：
/// 1. 6 位纯数字 → 先查 `stocks.code`；本地命中即返。**本地未命中时**回退到实时
///    报价探测——`quotes::fetch_a_share_quotes` 用的 ulist.np 接口与 clist 不同
///    源，stocks 表因 EM clist 风控而空时仍能验证 code 真实存在 + 取到 name。
/// 2. 否则按名字模糊匹配（`db::find_stocks_by_name`，内部精确优先 + LIKE 兜底）。
/// 3. 多结果 → 返带候选清单的歧义错误，让 agent 重新指定。
/// 4. 零结果 → "找不到"错误。
///
/// 调用方：agent 工具（get_quote / get_kline / get_indicators / open_position / scale_position）。
pub async fn resolve_stock(app: &AppHandle, identifier: &str) -> Result<StockRef, String> {
    let trimmed = identifier.trim();
    if trimmed.is_empty() {
        return Err("identifier 为空".into());
    }
    // 6 位纯数字 → 先查本地档案，未命中时回退到实时报价探测
    if trimmed.len() == 6 && trimmed.chars().all(|c| c.is_ascii_digit()) {
        if let Some(row) = crate::infrastructure::quotes::repository::find_stock_by_code(app, trimmed)? {
            return Ok(row.into());
        }
        return resolve_by_quote_probe(trimmed).await;
    }
    // 名字模糊（仍需本地 stocks 表——名字→代码的反向映射 ulist.np 给不了）
    let matches = crate::infrastructure::quotes::repository::find_stocks_by_name(app, trimmed, 6)?;
    if matches.is_empty() {
        return Err(format!(
            "找不到与 '{identifier}' 匹配的 A 股股票（请用 6 位代码或更完整的名字；本地清单可能未刷新）"
        ));
    }
    if matches.len() == 1 {
        return Ok(matches.into_iter().next().unwrap().into());
    }
    let candidates: Vec<String> = matches
        .iter()
        .map(|s| format!("{}（{}）", s.name, s.code))
        .collect();
    Err(format!(
        "'{identifier}' 匹配多个：{}——请用 6 位代码或更精确的名字",
        candidates.join("、")
    ))
}

/// 本地 stocks 表未命中时的兜底——用实时报价 ulist.np 接口探测 code 存在性。
/// 拿到价 + 名字就构造一个 StockRef（sector 留空，name 来自报价）。
///
/// 这条 path 不依赖 EM clist 端点，所以 EM 全市场扫描接口被风控时仍可用。
/// 由现有 `fetch_a_share_quotes` 的三源并发 fallback 自然继承稳健性。
async fn resolve_by_quote_probe(code: &str) -> Result<StockRef, String> {
    let parsed = StockCode::new(code).map_err(|e| e.to_string())?;
    let secid = parsed.to_em_secid();
    let pairs = em_realtime::fetch_quotes_by_secids(&[secid])
        .await
        .map_err(|e| {
            format!("代码 {code} 本地清单未命中，实时报价探测也失败：{e}（清单暂未刷新，稍后再试）")
        })?;
    let (_, q) = pairs.into_iter().next().ok_or_else(|| {
        format!("代码 {code} 在实时报价里也找不到——可能是非 A 股代码、已退市或代码写错")
    })?;
    Ok(StockRef {
        code: code.to_string(),
        name: q.name,
        sector: None,
        market: market_prefix(code).to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn market_prefix_buckets() {
        assert_eq!(market_prefix("600519"), "sh"); // 沪主板
        assert_eq!(market_prefix("000001"), "sz"); // 平安银行（个股语义）
        assert_eq!(market_prefix("000002"), "sz"); // 深主板
        assert_eq!(market_prefix("300750"), "sz"); // 创业板
        assert_eq!(market_prefix("688981"), "sh"); // 科创板
        assert_eq!(market_prefix("430564"), "bj"); // 北交所
        assert_eq!(market_prefix("832149"), "bj"); // 北交所
    }
}
