//! 行情类工具——3 个：get_quote / get_kline / get_market_overview。
//!
//! 所有数据走 quotes 模块的 snapshot-first 路径：
//! - `infrastructure::quotes::snapshot::market_snapshot`（scheduler 维护，同步读）
//! - `infrastructure::quotes::cache::kline_cache`（K 线持久化缓存）
//! - `pipeline::market_overview`（大盘指数拼装）

use crate::adapters::quotes_commands::StockQuoteDto;
use crate::infrastructure::agent::tools::{err_text, ok_json, Tool, ToolContext};
use crate::domain::agent::types::ToolResultContent;
use crate::domain::quotes::KlinePeriod;
use crate::infrastructure::quotes::cache::kline_cache::{self, Category};
use crate::infrastructure::quotes::snapshot::market_snapshot;
use crate::pipeline::market_overview;
use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::AppHandle;

// ===== 输入校验 ==========================================================

async fn parse_code(input: &Value, app: &AppHandle) -> Result<String, String> {
    let raw = input
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing code".to_string())?
        .trim();
    if raw.is_empty() {
        return Err("code 为空".into());
    }
    let stock = crate::pipeline::stocks::resolve_stock(app, raw).await?;
    Ok(stock.code)
}

fn parse_period_enum(input: &Value) -> KlinePeriod {
    match input.get("period").and_then(Value::as_str) {
        Some("week") => KlinePeriod::Week,
        Some("month") => KlinePeriod::Month,
        _ => KlinePeriod::Day,
    }
}

fn period_label(p: KlinePeriod) -> &'static str {
    match p {
        KlinePeriod::Day => "day",
        KlinePeriod::Week => "week",
        KlinePeriod::Month => "month",
    }
}

/// 6 位 code → ts_code，**走 stocks 表 lookup**（TuShare 权威 market），不前缀猜测。
/// 未命中（新股 / 表空 / 退市）返 None——caller 应该提示用户档案待刷新。
fn resolve_stock_ts_code(app: &AppHandle, code: &str) -> Option<String> {
    crate::infrastructure::quotes::repository::resolve_stock_ts_code(app, code)
}

// ===== get_quote =========================================================

pub struct GetQuoteTool {
    app: AppHandle,
}

impl GetQuoteTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for GetQuoteTool {
    fn name(&self) -> &'static str {
        "get_quote"
    }

    fn description(&self) -> &'static str {
        "获取 A 股某只股票的实时行情快照（最新价、涨跌幅、成交量、开盘/最高/最低）。\
        判断当前价位、是否突破压力位、当日异动时调用。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "6 位 A 股代码或股票中文名" }
            },
            "required": ["code"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let code = match parse_code(&input, &self.app).await {
            Ok(c) => c,
            Err(e) => return err_text(e),
        };
        let ts_code = match crate::infrastructure::quotes::repository::resolve_stock_ts_code(&self.app, &code) {
            Some(ts) => ts,
            None => {
                return err_text(format!(
                    "stocks 档案找不到 {code}——新股 / 已退市 / 档案未刷新"
                ))
            }
        };
        // 优先读 MARKET_SNAPSHOT；缺则 lazy ensure 单只（走 dispatch 多源 fallback）
        let quote = match market_snapshot::get(&ts_code) {
            Some(q) => q,
            None => {
                match crate::infrastructure::quotes::realtime::dispatch()
                    .fetch(&[ts_code.clone()])
                    .await
                {
                    Ok(mut v) => {
                        market_snapshot::put_batch(v.clone());
                        match v.pop() {
                            Some((_, q)) => q,
                            None => {
                                return err_text(format!("{code} 实时报价为空（可能停牌 / 退市）"))
                            }
                        }
                    }
                    Err(e) => return err_text(format!("get_quote 拉取失败：{e}")),
                }
            }
        };
        match serde_json::to_value(StockQuoteDto::from(quote)) {
            Ok(json) => (ok_json(json), false),
            Err(e) => err_text(format!("序列化失败：{e}")),
        }
    }
}

// ===== get_kline =========================================================

pub struct GetKlineTool {
    app: AppHandle,
}

impl GetKlineTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for GetKlineTool {
    fn name(&self) -> &'static str {
        "get_kline"
    }

    fn description(&self) -> &'static str {
        "拉个股日/周/月 K 线（OHLC + 成交量/额）。判断趋势、识别形态、回顾历史走势时调用。\
        本地缓存优先（< 5ms），过期时同步增量拉 TuShare。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code":   { "type": "string", "description": "6 位 A 股代码或股票中文名" },
                "period": { "type": "string", "enum": ["day", "week", "month"], "default": "day" },
                "limit":  { "type": "integer", "minimum": 30, "maximum": 800, "default": 120 }
            },
            "required": ["code"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let code = match parse_code(&input, &self.app).await {
            Ok(c) => c,
            Err(e) => return err_text(e),
        };
        let period = parse_period_enum(&input);
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(120)
            .clamp(30, 800);
        let ts_code = match resolve_stock_ts_code(&self.app, &code) {
            Some(ts) => ts,
            None => {
                return err_text(format!(
                    "stocks 档案里找不到 {code}——可能是新股 / 已退市，或档案表暂未刷新"
                ));
            }
        };

        let rows = match kline_cache::get_or_refresh(
            &self.app,
            &ts_code,
            Category::Stock,
            period,
            Category::Stock.default_adj(),
            limit,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => return err_text(format!("get_kline 拉取失败：{e}")),
        };
        if rows.is_empty() {
            return err_text(format!("{code} 无 K 线数据"));
        }
        let klines: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "date": format_iso(&r.date),
                    "open": r.open,
                    "close": r.close,
                    "high": r.high,
                    "low": r.low,
                    "volume": r.volume,
                    "amount": r.amount,
                })
            })
            .collect();
        let payload = json!({
            "code": code,
            "period": period_label(period),
            "count": klines.len(),
            "klines": klines,
        });
        (ok_json(payload), false)
    }
}

fn format_iso(compact: &str) -> String {
    if compact.len() == 8 {
        format!("{}-{}-{}", &compact[0..4], &compact[4..6], &compact[6..8])
    } else {
        compact.to_string()
    }
}

// ===== get_market_overview ===============================================

pub struct GetMarketOverviewTool {
    app: AppHandle,
}

impl GetMarketOverviewTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for GetMarketOverviewTool {
    fn name(&self) -> &'static str {
        "get_market_overview"
    }

    fn description(&self) -> &'static str {
        "大盘指数（上证、深证、创业板、科创 50）快照。判断风险偏好、行业轮动时调用。\
        breadth + sectors 字段当前为空（数据源重构中）。"
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        match market_overview::fetch_market_overview(&self.app).await {
            Ok(o) => match serde_json::to_value(
                crate::adapters::quotes_commands::MarketOverviewDto::from(o),
            ) {
                Ok(v) => (ok_json(v), false),
                Err(e) => err_text(format!("序列化失败：{e}")),
            },
            Err(e) => err_text(format!("get_market_overview 拉取失败：{e}")),
        }
    }
}
