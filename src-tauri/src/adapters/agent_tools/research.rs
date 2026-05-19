//! 研究类工具——agent 在 chat 里做"为什么这只这个时候动"分析时用。
//!
//! 5 个工具：
//! - `scan_market`：扫盘榜单（涨停/跌停/涨幅/跌幅/成交额/成交量 top N）
//! - `get_top_list`：龙虎榜（上榜股票的成交额 / 净买入 / 上榜原因）
//! - `get_moneyflow`：个股资金流（小/中/大/特大单分级净流入）
//! - `get_concept_performance`：板块涨幅排行
//! - `get_company_events`：公司事件（公告 / 分红 / 解禁 / 财报）
//!
//! 全部调 `infrastructure/quotes` 的现成接口。错误统一映射成 is_error=true 让 agent
//! 看到失败描述（接口降级 / 缺 token / 日期不合规等）。

use crate::pipeline::agent::tools::{err_text, ok_json, Tool, ToolContext};
use crate::domain::agent::types::ToolResultContent;
use crate::domain::quotes::{ScanFilter, ScanItem, ScanResult};
use crate::domain::shared::{StockCode, TradeDate};
use crate::infrastructure::quotes::scanner;
use crate::infrastructure::quotes::tushare::{calendar, concept, events, flow};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tauri::AppHandle;

// ===== 共用 helpers =======================================================

fn parse_optional_trade_date(s: Option<&str>) -> Result<Option<TradeDate>, String> {
    match s {
        None => Ok(None),
        Some(raw) => {
            let t = raw.trim();
            if t.is_empty() {
                Ok(None)
            } else {
                TradeDate::from_compact(t)
                    .or_else(|_| TradeDate::from_iso(t))
                    .map(Some)
                    .map_err(|e| format!("trade_date 解析失败：{e}"))
            }
        }
    }
}

/// 把 ScanItem 序列化成给 agent 看的精简 JSON。
///
/// 默认 6 个字段：rank/code/name/price/change_pct/amount——真人扫榜决策需要的核心。
/// verbose=true 时补全 turnover_rate/volume_ratio/pe/pb/total_mv（量化筛选用）。
/// None 字段一律 skip，减少 `"pe": null` 这种纯噪声。
fn compact_scan_item(item: &ScanItem, verbose: bool) -> Value {
    let mut m = Map::new();
    m.insert("rank".into(), json!(item.rank));
    m.insert("code".into(), json!(item.code.as_str()));
    m.insert("name".into(), json!(item.name));
    if let Some(p) = item.price.as_ref() {
        m.insert("price".into(), json!(p.value()));
    }
    if let Some(c) = item.change_pct {
        m.insert("change_pct".into(), json!(c));
    }
    if let Some(a) = item.amount.as_ref() {
        m.insert("amount".into(), json!(a.value()));
    }
    if verbose {
        if let Some(v) = item.volume.as_ref() {
            m.insert("volume".into(), json!(v.value()));
        }
        if let Some(t) = item.turnover_rate {
            m.insert("turnover_rate".into(), json!(t));
        }
        if let Some(vr) = item.volume_ratio {
            m.insert("volume_ratio".into(), json!(vr));
        }
        if let Some(pe) = item.pe {
            m.insert("pe".into(), json!(pe));
        }
        if let Some(pb) = item.pb {
            m.insert("pb".into(), json!(pb));
        }
        if let Some(mv) = item.total_mv.as_ref() {
            m.insert("total_mv".into(), json!(mv.value()));
        }
    }
    Value::Object(m)
}

fn compact_scan_result(result: &ScanResult, verbose: bool) -> Value {
    json!({
        "trade_date": result.trade_date.to_iso(),
        "items": result.items.iter().map(|i| compact_scan_item(i, verbose)).collect::<Vec<_>>(),
    })
}

fn parse_scan_filter(s: &str) -> Result<ScanFilter, String> {
    match s {
        "limit_up" => Ok(ScanFilter::LimitUp),
        "limit_down" => Ok(ScanFilter::LimitDown),
        "top_gain" => Ok(ScanFilter::TopGain),
        "top_loss" => Ok(ScanFilter::TopLoss),
        "top_amount" => Ok(ScanFilter::TopAmount),
        "top_volume" => Ok(ScanFilter::TopVolume),
        other => Err(format!(
            "未知 filter：{other}（应为 limit_up / limit_down / top_gain / top_loss / top_amount / top_volume）"
        )),
    }
}

// ===== scan_market ========================================================

pub struct ScanMarketTool {
    app: AppHandle,
}

impl ScanMarketTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for ScanMarketTool {
    fn name(&self) -> &'static str {
        "scan_market"
    }

    fn description(&self) -> &'static str {
        "扫 A 股榜单（涨停/跌停/涨幅/跌幅/成交额/成交量）。盘后数据非实时。\
        verbose=true 加 pe/pb/turnover_rate 等量化字段。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "string",
                    "enum": ["limit_up", "limit_down", "top_gain", "top_loss", "top_amount", "top_volume"],
                    "description": "limit_up 涨停 / limit_down 跌停 / top_gain 涨幅榜 / top_loss 跌幅榜 / top_amount 成交额榜 / top_volume 成交量榜"
                },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 },
                "verbose": {
                    "type": "boolean",
                    "default": false,
                    "description": "true 时额外返回 volume/turnover_rate/volume_ratio/pe/pb/total_mv（量化筛选用）"
                }
            },
            "required": ["filter"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let filter_str = input.get("filter").and_then(Value::as_str).unwrap_or("");
        let filter = match parse_scan_filter(filter_str) {
            Ok(f) => f,
            Err(e) => return err_text(e),
        };
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .clamp(1, 100) as usize;
        let verbose = input
            .get("verbose")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match scanner::scan_market(&self.app, filter, limit).await {
            Ok(result) => (ok_json(compact_scan_result(&result, verbose)), false),
            Err(e) => err_text(format!("scan_market 失败：{e}")),
        }
    }
}

// ===== get_top_list =======================================================

pub struct GetTopListTool {
    app: AppHandle,
}

impl GetTopListTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for GetTopListTool {
    fn name(&self) -> &'static str {
        "get_top_list"
    }

    fn description(&self) -> &'static str {
        "龙虎榜——主力成交 / 净买入 / 上榜原因。trade_date 留空 = 最近交易日。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "trade_date": {
                    "type": "string",
                    "description": "YYYYMMDD 或 YYYY-MM-DD；留空 = 最近一个交易日"
                },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 30 }
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let date_str = input.get("trade_date").and_then(Value::as_str);
        let date = match parse_optional_trade_date(date_str) {
            Ok(d) => d,
            Err(e) => return err_text(e),
        };
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(30)
            .clamp(1, 100) as usize;
        match flow::fetch_top_list(&self.app, date).await {
            Ok(mut items) => {
                // 按 net_amount 绝对值排序，截 top N——比按原始顺序更有信息密度
                items.sort_by(|a, b| {
                    let av = a.net_amount.as_ref().map(|y| y.value().abs()).unwrap_or(0.0);
                    let bv = b.net_amount.as_ref().map(|y| y.value().abs()).unwrap_or(0.0);
                    bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
                });
                items.truncate(limit);
                match serde_json::to_value(&items) {
                    Ok(v) => (ok_json(v), false),
                    Err(e) => err_text(format!("序列化失败：{e}")),
                }
            }
            Err(e) => err_text(format!("get_top_list 失败：{e}")),
        }
    }
}

// ===== get_moneyflow ======================================================

pub struct GetMoneyflowTool {
    app: AppHandle,
}

impl GetMoneyflowTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for GetMoneyflowTool {
    fn name(&self) -> &'static str {
        "get_moneyflow"
    }

    fn description(&self) -> &'static str {
        "个股资金流（小/中/大/特大单净流入）。特大单（>100 万）通常代表主力动向。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "6 位 A 股代码或中文名" },
                "days": { "type": "integer", "minimum": 1, "maximum": 60, "default": 10 }
            },
            "required": ["code"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let raw = input
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if raw.is_empty() {
            return err_text("code 为空");
        }
        let stock = match crate::pipeline::stocks::resolve_stock(&self.app, raw).await {
            Ok(s) => s,
            Err(e) => return err_text(format!("code 解析失败：{e}")),
        };
        let code = match StockCode::new(&stock.code) {
            Ok(c) => c,
            Err(e) => return err_text(format!("非法 code：{e}")),
        };
        let days = input
            .get("days")
            .and_then(Value::as_u64)
            .unwrap_or(10)
            .clamp(1, 60) as usize;
        match flow::fetch_moneyflow(&self.app, &code, days).await {
            Ok(items) => match serde_json::to_value(&items) {
                Ok(v) => (ok_json(v), false),
                Err(e) => err_text(format!("序列化失败：{e}")),
            },
            Err(e) => err_text(format!("get_moneyflow 失败：{e}")),
        }
    }
}

// ===== get_concept_performance ============================================

pub struct GetConceptPerformanceTool {
    app: AppHandle,
}

impl GetConceptPerformanceTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for GetConceptPerformanceTool {
    fn name(&self) -> &'static str {
        "get_concept_performance"
    }

    fn description(&self) -> &'static str {
        "概念板块涨幅 top 20（含成交额 / 成分股数）。判断热点轮动时调。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "trade_date": {
                    "type": "string",
                    "description": "YYYYMMDD 或 YYYY-MM-DD；留空 = 最近一个交易日"
                },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 }
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let date_str = input.get("trade_date").and_then(Value::as_str);
        let date = match parse_optional_trade_date(date_str) {
            Ok(Some(d)) => d,
            Ok(None) => calendar::current_trade_date(),
            Err(e) => return err_text(e),
        };
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .clamp(1, 100) as usize;
        match concept::fetch_concept_performance(&self.app, date).await {
            Ok(mut items) => {
                // fetch_concept_performance 已按涨幅降序——截 top N
                items.truncate(limit);
                match serde_json::to_value(&items) {
                    Ok(v) => (ok_json(v), false),
                    Err(e) => err_text(format!("序列化失败：{e}")),
                }
            }
            Err(e) => err_text(format!("get_concept_performance 失败：{e}")),
        }
    }
}

// ===== get_company_events =================================================

pub struct GetCompanyEventsTool {
    app: AppHandle,
}

impl GetCompanyEventsTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for GetCompanyEventsTool {
    fn name(&self) -> &'static str {
        "get_company_events"
    }

    fn description(&self) -> &'static str {
        "公司事件（未来 N 天的分红 / 解禁 / 财报 / 股东大会）。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "6 位 A 股代码或中文名" },
                "days_ahead": { "type": "integer", "minimum": 1, "maximum": 180, "default": 30 }
            },
            "required": ["code"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let raw = input
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if raw.is_empty() {
            return err_text("code 为空");
        }
        let stock = match crate::pipeline::stocks::resolve_stock(&self.app, raw).await {
            Ok(s) => s,
            Err(e) => return err_text(format!("code 解析失败：{e}")),
        };
        let code = match StockCode::new(&stock.code) {
            Ok(c) => c,
            Err(e) => return err_text(format!("非法 code：{e}")),
        };
        let days = input
            .get("days_ahead")
            .and_then(Value::as_i64)
            .unwrap_or(30)
            .clamp(1, 180) as i32;
        match events::fetch_company_events(&self.app, &code, days).await {
            Ok(items) => match serde_json::to_value(&items) {
                Ok(v) => (ok_json(v), false),
                Err(e) => err_text(format!("序列化失败：{e}")),
            },
            Err(e) => err_text(format!("get_company_events 失败：{e}")),
        }
    }
}
