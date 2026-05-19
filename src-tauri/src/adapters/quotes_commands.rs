//! Tauri commands——KlineChart 主路径专用。
//!
//! 这层把 domain 类型（StockQuote / KlineSeries / MinutePoint）转成前端
//! 期望的 JSON 格式，**不破坏现有前端 invoke 签名**。
//!
//! 前端调用：
//! - `invoke("fetch_a_share_klines", { code, period, limit })` → KlinePointDto[]
//! - `invoke("fetch_a_share_minutes", { tsCode, days })` → MinutePointDto[]
//!
//! 内部走 `infrastructure::tushare::stock::fetch_klines` + `infrastructure::eastmoney::*`，
//! 全部用 domain 新 types（StockCode / Yuan / Lots / TradeDate）；输出时转 f64/String
//! 保持前端兼容。

use crate::domain::quotes::{
    CompanyEvent, ConceptPerformance, ConceptSector, KlinePeriod, ListStatus, MarginSummary,
    MoneyFlowItem, NorthHolding, NorthMoneyFlow, ScanCondition, ScanFilter, ScanResult, ScanSort,
    StockProfile, TopListItem,
};
use crate::domain::shared::{StockCode, TradeDate, TsCode};
use crate::infrastructure::quotes::cache::kline_cache::{self, Category};
use crate::infrastructure::quotes::eastmoney::kline as em_kline;
use crate::infrastructure::quotes::realtime::dispatch;
use crate::infrastructure::quotes::scanner as quotes_scanner;
use crate::infrastructure::quotes::tdx::bars as tdx_bars;
use crate::infrastructure::quotes::tushare::probe::ProbeResult;
use crate::infrastructure::quotes::tushare::{concept, events, flow, stock as ts_stock};
use serde::Serialize;

// ============================================================================
// DTO（前端兼容输出）
// ============================================================================

/// K 线点——前端 KlineChart.tsx 期望的字段命名。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KlinePointDto {
    pub date: String,
    pub open: f64,
    pub close: f64,
    pub high: f64,
    pub low: f64,
    pub volume: Option<f64>,
    pub amount: Option<f64>,
}

/// 分时点——前端期望的字段。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MinutePointDto {
    pub time: String,
    pub price: f64,
    pub average: Option<f64>,
    pub volume: Option<f64>,
    pub amount: Option<f64>,
}

// ============================================================================
// Tauri commands
// ============================================================================

/// 拉历史 K 线——前端 KlineChart 调用。**Snapshot-First** + **ts_code 入口**：
///
/// 1. 前端传 ts_code（"000001.SZ" / "510300.SH" / "920469.BJ"）—— 唯一标识，无歧义
/// 2. category 显式传（个股/指数/基金）—— 决定调 TuShare 哪个接口
/// 3. 检查 kline_meta TTL（10 分钟）+ klines 表是否有数据
/// 4. 命中 → 直接读 SQLite 返
/// 5. 未命中 / 过期 → 阻塞 ensure（增量拉外部 + 写 DB）→ 再读 SQLite 返
#[tauri::command]
pub async fn fetch_a_share_klines(
    app: tauri::AppHandle,
    ts_code: String,
    period: Option<String>,
    limit: Option<usize>,
    category: Option<String>,
) -> Result<Vec<KlinePointDto>, String> {
    let ts = TsCode::new(&ts_code).map_err(|e| e.to_string())?;
    let period_enum = match period.as_deref().unwrap_or("day") {
        "day" => KlinePeriod::Day,
        "week" => KlinePeriod::Week,
        "month" => KlinePeriod::Month,
        other => return Err(format!("不支持的 K 线周期：{other}")),
    };
    let limit = limit.unwrap_or(200).clamp(30, 800);
    let cat = Category::from_str(category.as_deref().unwrap_or("stock"));
    let adj = cat.default_adj();

    // 走缓存层——10 分钟 TTL 内直接读 DB；过期 / 缺失则阻塞 ensure 后再读
    let rows = kline_cache::get_or_refresh(&app, ts.as_str(), cat, period_enum, adj, limit).await?;

    Ok(rows
        .into_iter()
        .map(|r| KlinePointDto {
            date: format_iso(&r.date),
            open: r.open,
            close: r.close,
            high: r.high,
            low: r.low,
            volume: r.volume,
            amount: r.amount,
        })
        .collect())
}

/// "20250513" → "2025-05-13"
fn format_iso(compact: &str) -> String {
    if compact.len() == 8 {
        format!("{}-{}-{}", &compact[0..4], &compact[4..6], &compact[6..8])
    } else {
        compact.to_string()
    }
}

/// 拉当日分时图——走 EM push2his trends2 端点。
///
/// `days` 默认 1，clamp 1..=5。
#[tauri::command]
pub async fn fetch_a_share_minutes(
    ts_code: Option<String>,
    code: Option<String>,
    days: Option<usize>,
) -> Result<Vec<MinutePointDto>, String> {
    let ts_code = match ts_code {
        Some(value) => TsCode::new(value).map_err(|e| e.to_string())?,
        None => {
            let code = StockCode::new(code.ok_or_else(|| "缺少 tsCode".to_string())?)
                .map_err(|e| e.to_string())?;
            TsCode::new(code.to_ts_code()).map_err(|e| e.to_string())?
        }
    };
    let days = days.unwrap_or(1);

    // TDX 主 + EM 兜底。SH/SZ 先试 TDX；BJ 或 TDX 失败/空 → EM。
    let points = fetch_minutes_with_fallback(&ts_code, days).await?;

    Ok(points
        .into_iter()
        .map(|p| MinutePointDto {
            time: p.time,
            price: p.price.value(),
            average: p.average.map(|v| v.value()),
            volume: p.volume.map(|v| v.value() as f64),
            amount: p.amount.map(|v| v.value()),
        })
        .collect())
}

/// 分时数据 TDX 主 + EM 兜底——同 minute_kline 路径策略。
/// 用 String 错误是因为这是 adapter 层；上游 caller 直接面向前端。
async fn fetch_minutes_with_fallback(
    ts_code: &TsCode,
    days: usize,
) -> Result<Vec<crate::domain::quotes::MinutePoint>, String> {
    let is_bj = ts_code.market() == "BJ";
    if !is_bj {
        match tdx_bars::fetch_minutes_intraday(ts_code, days).await {
            Ok(pts) if !pts.is_empty() => {
                tracing::debug!(ts_code = %ts_code.as_str(), source = "tdx", "分时命中");
                return Ok(pts);
            }
            Ok(_) => tracing::debug!(ts_code = %ts_code.as_str(), "tdx 分时返 0，回退 EM"),
            Err(e) => tracing::warn!(ts_code = %ts_code.as_str(), err = %e, "tdx 分时失败，回退 EM"),
        }
    }
    em_kline::fetch_minutes_intraday(ts_code, days)
        .await
        .map_err(String::from)
}

// ============================================================================
// 分钟 K（1m/5m/15m/30m/60m）—— Snapshot-First：DB cache 优先，TTL 内同步读
// ============================================================================

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MinuteKlinePointDto {
    pub timestamp: i64, // unix ms
    pub open: f64,
    pub close: f64,
    pub high: f64,
    pub low: f64,
    pub volume: i64,
    pub amount: f64,
}

#[tauri::command]
pub async fn fetch_minute_klines(
    app: tauri::AppHandle,
    ts_code: String,
    period: String, // "1m" | "5m" | "15m" | "30m" | "60m"
    limit: Option<usize>,
) -> Result<Vec<MinuteKlinePointDto>, String> {
    use crate::infrastructure::quotes::cache::minute_kline_cache;
    let period_enum = minute_kline_cache::parse_period(&period)
        .ok_or_else(|| format!("不支持的分钟周期：{period}"))?;
    let limit = limit.unwrap_or(240).clamp(30, 800);

    let rows = minute_kline_cache::get_or_refresh(&app, &ts_code, period_enum, limit).await?;

    Ok(rows
        .into_iter()
        .map(|r| MinuteKlinePointDto {
            timestamp: r.timestamp_ms,
            open: r.open,
            close: r.close,
            high: r.high,
            low: r.low,
            volume: r.volume,
            amount: r.amount,
        })
        .collect())
}

// ============================================================================
// 实时报价（基础字段 + 可选五档）—— 来自 MARKET_SNAPSHOT
// ============================================================================
//
// TDX 主路径会填五档；EM / 腾讯 / 新浪 fallback 只填基础字段，五档为空。

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StockQuoteDto {
    pub code: String,
    pub name: String,
    pub price: Option<f64>,
    pub change_percent: Option<f64>,
    pub change: Option<f64>,
    pub open: Option<f64>,
    pub high: Option<f64>,
    pub low: Option<f64>,
    pub previous_close: Option<f64>,
    pub day_volume: Option<f64>, // 当日成交量（手）
    pub day_amount: Option<f64>, // 当日成交额（元）
    pub captured_at: i64,        // 本地拉取时间（unix ms）
    pub bid_levels: Vec<OrderBookLevelDto>,
    pub ask_levels: Vec<OrderBookLevelDto>,
    pub buy_volume: Option<f64>,
    pub sell_volume: Option<f64>,
    pub order_imbalance: Option<f64>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OrderBookLevelDto {
    pub price: Option<f64>,
    pub volume: Option<f64>,
}

impl From<crate::domain::quotes::StockQuote> for StockQuoteDto {
    fn from(q: crate::domain::quotes::StockQuote) -> Self {
        Self {
            code: q.code.as_str().to_string(),
            name: q.name,
            price: q.price.map(|v| v.value()),
            change_percent: q.change_percent,
            change: q.change.map(|v| v.value()),
            open: q.open.map(|v| v.value()),
            high: q.high.map(|v| v.value()),
            low: q.low.map(|v| v.value()),
            previous_close: q.previous_close.map(|v| v.value()),
            day_volume: q.day_volume.map(|v| v.value() as f64),
            day_amount: q.day_amount.map(|v| v.value()),
            captured_at: q.captured_at.value(),
            bid_levels: q.bid_levels.into_iter().map(Into::into).collect(),
            ask_levels: q.ask_levels.into_iter().map(Into::into).collect(),
            buy_volume: q.buy_volume.map(|v| v.value() as f64),
            sell_volume: q.sell_volume.map(|v| v.value() as f64),
            order_imbalance: q.order_imbalance,
        }
    }
}

impl From<crate::domain::quotes::OrderBookLevel> for OrderBookLevelDto {
    fn from(level: crate::domain::quotes::OrderBookLevel) -> Self {
        Self {
            price: level.price.map(|v| v.value()),
            volume: level.volume.map(|v| v.value() as f64),
        }
    }
}

// ============================================================================
// MarketOverview DTO（前端 types.ts 契约）
// ============================================================================

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MarketIndexDto {
    pub code: String,
    pub name: String,
    pub price: Option<f64>,
    pub change: Option<f64>,
    pub change_percent: Option<f64>,
    pub captured_at: i64, // unix ms
}

impl From<crate::domain::quotes::MarketIndex> for MarketIndexDto {
    fn from(i: crate::domain::quotes::MarketIndex) -> Self {
        Self {
            code: i.code.as_str().to_string(),
            name: i.name,
            price: i.price.map(|v| v.value()),
            change: i.change.map(|v| v.value()),
            change_percent: i.change_percent,
            captured_at: i.timestamp.value(),
        }
    }
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MarketBreadthDto {
    pub rise: u32,
    pub fall: u32,
    pub flat: u32,
}

impl From<crate::domain::quotes::MarketBreadth> for MarketBreadthDto {
    fn from(b: crate::domain::quotes::MarketBreadth) -> Self {
        Self {
            rise: b.rise,
            fall: b.fall,
            flat: b.flat,
        }
    }
}

/// 行业板块涨跌——前端"行业热度"卡片消费。domain 暂无此类型，DTO 独立存在
/// （pipeline 拉到的时候直接构造）。
#[derive(Debug, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct SectorHotDto {
    pub code: String,
    pub name: String,
    pub change_percent: Option<f64>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MarketOverviewDto {
    pub indices: Vec<MarketIndexDto>,
    pub breadth: MarketBreadthDto,
    pub sectors: Vec<SectorHotDto>,
    pub captured_at: i64,
}

impl From<crate::domain::quotes::MarketOverview> for MarketOverviewDto {
    fn from(o: crate::domain::quotes::MarketOverview) -> Self {
        Self {
            indices: o.indices.into_iter().map(Into::into).collect(),
            breadth: o.breadth.into(),
            sectors: Vec::new(), // domain 暂无 sectors，行业热度下一轮接通
            captured_at: o.timestamp.value(),
        }
    }
}

/// 实时报价（基础字段）—— **Snapshot-First**：直接读 MARKET_SNAPSHOT。
///
/// 数据来源：`market_quote_loop` scheduler 刷新，写入 MARKET_SNAPSHOT。
/// 缺失的 code（极少数）→ 单次 lazy ensure 后写入 snapshot。
///
/// 参数：`codes` 是 6 位列表；通过 stocks 表查 ts_code 作为 snapshot key。
#[tauri::command]
pub async fn fetch_a_share_quotes(
    app: tauri::AppHandle,
    codes: Vec<String>,
) -> Result<Vec<StockQuoteDto>, String> {
    use crate::infrastructure::quotes::snapshot::market_snapshot;
    if codes.is_empty() {
        return Ok(Vec::new());
    }

    // 1. 6 位 code → ts_code（走 stocks 表 lookup，权威 market）
    let ts_codes: Vec<String> = codes
        .iter()
        .filter_map(|c| crate::infrastructure::quotes::repository::resolve_stock_ts_code(&app, c))
        .collect();

    // 2. 读 MARKET_SNAPSHOT；缺失的同步 ensure 一次（走 dispatch 多源 fallback）
    let mut found: Vec<crate::domain::quotes::StockQuote> = Vec::with_capacity(ts_codes.len());
    let mut missing_ts: Vec<String> = Vec::new();
    for ts in &ts_codes {
        if let Some(q) = market_snapshot::get(ts) {
            found.push(q);
        } else {
            missing_ts.push(ts.clone());
        }
    }

    if !missing_ts.is_empty() {
        match dispatch().fetch(&missing_ts).await {
            Ok(fresh) => {
                market_snapshot::put_batch(fresh.clone());
                for (_, q) in fresh {
                    found.push(q);
                }
            }
            Err(e) => tracing::warn!(err = %e, "ensure quote lazy 失败，返当前 snapshot"),
        }
    }

    Ok(found.into_iter().map(StockQuoteDto::from).collect())
}

// ============================================================================
// get_market_overview —— 大盘指数快照（4 大指数 + breadth + sectors）
// ============================================================================

#[tauri::command]
pub async fn get_market_overview(app: tauri::AppHandle) -> Result<MarketOverviewDto, String> {
    let domain = crate::pipeline::market::overview::fetch_market_overview(&app).await?;
    Ok(domain.into())
}

// ============================================================================
// Quotes research APIs——资金面 / 公司动作 / 概念 / scanner
// ============================================================================

#[tauri::command]
pub async fn fetch_top_list(
    app: tauri::AppHandle,
    trade_date: Option<String>,
) -> Result<Vec<TopListItem>, String> {
    let date = parse_optional_trade_date(trade_date)?;
    flow::fetch_top_list(&app, date).await.map_err(String::from)
}

#[tauri::command]
pub async fn fetch_moneyflow(
    app: tauri::AppHandle,
    code: String,
    days: Option<usize>,
) -> Result<Vec<MoneyFlowItem>, String> {
    let code = StockCode::new(code).map_err(|e| e.to_string())?;
    flow::fetch_moneyflow(&app, &code, days.unwrap_or(20).clamp(1, 120))
        .await
        .map_err(String::from)
}

#[tauri::command]
pub async fn fetch_north_flow(
    app: tauri::AppHandle,
    days: Option<usize>,
) -> Result<Vec<NorthMoneyFlow>, String> {
    flow::fetch_north_flow(&app, days.unwrap_or(20).clamp(1, 120))
        .await
        .map_err(String::from)
}

#[tauri::command]
pub async fn fetch_north_top10(
    app: tauri::AppHandle,
    trade_date: Option<String>,
) -> Result<Vec<NorthHolding>, String> {
    let date = parse_optional_trade_date(trade_date)?
        .unwrap_or_else(crate::infrastructure::quotes::tushare::calendar::current_trade_date);
    flow::fetch_north_top10(&app, date)
        .await
        .map_err(String::from)
}

#[tauri::command]
pub async fn fetch_margin_summary(
    app: tauri::AppHandle,
    days: Option<usize>,
) -> Result<Vec<MarginSummary>, String> {
    flow::fetch_margin_summary(&app, days.unwrap_or(20).clamp(1, 120))
        .await
        .map_err(String::from)
}

#[tauri::command]
pub async fn fetch_company_events(
    app: tauri::AppHandle,
    code: String,
    days_ahead: Option<i32>,
) -> Result<Vec<CompanyEvent>, String> {
    let code = StockCode::new(code).map_err(|e| e.to_string())?;
    events::fetch_company_events(&app, &code, days_ahead.unwrap_or(90))
        .await
        .map_err(String::from)
}

#[tauri::command]
pub async fn fetch_concept_list(app: tauri::AppHandle) -> Result<Vec<ConceptSector>, String> {
    concept::fetch_concept_list(&app)
        .await
        .map_err(String::from)
}

#[tauri::command]
pub async fn fetch_concept_members(
    app: tauri::AppHandle,
    concept_code: String,
) -> Result<Vec<String>, String> {
    concept::fetch_concept_members(&app, &concept_code)
        .await
        .map(|codes| codes.into_iter().map(|c| c.as_str().to_string()).collect())
        .map_err(String::from)
}

#[tauri::command]
pub async fn fetch_concept_performance(
    app: tauri::AppHandle,
    trade_date: Option<String>,
) -> Result<Vec<ConceptPerformance>, String> {
    let date = parse_optional_trade_date(trade_date)?
        .unwrap_or_else(crate::infrastructure::quotes::tushare::calendar::current_trade_date);
    concept::fetch_concept_performance(&app, date)
        .await
        .map_err(String::from)
}

#[tauri::command]
pub async fn scan_market(
    app: tauri::AppHandle,
    filter: ScanFilter,
    limit: Option<usize>,
) -> Result<ScanResult, String> {
    quotes_scanner::scan_market(&app, filter, limit.unwrap_or(50))
        .await
        .map_err(String::from)
}

#[tauri::command]
pub async fn scan_market_query(
    app: tauri::AppHandle,
    conditions: Vec<ScanCondition>,
    sort_by: ScanSort,
    limit: Option<usize>,
) -> Result<ScanResult, String> {
    quotes_scanner::scan_market_query(&app, conditions, sort_by, limit.unwrap_or(50))
        .await
        .map_err(String::from)
}

#[tauri::command]
pub async fn fetch_stock_profile(
    app: tauri::AppHandle,
    code: String,
) -> Result<StockProfile, String> {
    let code = StockCode::new(code).map_err(|e| e.to_string())?;
    let row = crate::infrastructure::quotes::repository::find_stock_by_code(&app, code.as_str())?
        .ok_or_else(|| format!("stocks 档案未找到 {}", code.as_str()))?;
    let fundamentals = ts_stock::fetch_daily_basic(&app, &code)
        .await
        .map_err(String::from)?;
    Ok(StockProfile {
        stock_ref: crate::domain::quotes::StockRef {
            code,
            name: row.name,
            sector: row.sector,
            market: row.market,
        },
        fundamentals,
        list_date: None,
        list_status: ListStatus::Listed,
        is_st: false,
    })
}

fn parse_optional_trade_date(input: Option<String>) -> Result<Option<TradeDate>, String> {
    match input.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) if s.contains('-') => TradeDate::from_iso(s).map(Some).map_err(|e| e.to_string()),
        Some(s) => TradeDate::from_compact(s)
            .map(Some)
            .map_err(|e| e.to_string()),
        None => Ok(None),
    }
}

#[tauri::command]
pub async fn save_tushare_token(app: tauri::AppHandle, token: String) -> Result<(), String> {
    crate::pipeline::stocks::save_tushare_token(app, token).await
}

#[tauri::command]
pub async fn probe_tushare_capabilities(app: tauri::AppHandle) -> Result<Vec<ProbeResult>, String> {
    crate::infrastructure::quotes::tushare::probe::probe_tushare_capabilities(app).await
}
