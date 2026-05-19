//! 行情类工具——3 个：get_quote / get_kline / get_market_overview。
//!
//! 所有数据走 quotes 模块的 snapshot-first 路径：
//! - `infrastructure::quotes::snapshot::market_snapshot`（scheduler 维护，同步读）
//! - `infrastructure::quotes::cache::kline_cache`（K 线持久化缓存）
//! - `pipeline::market::overview`（大盘指数拼装）

use crate::pipeline::agent::tools::{err_text, ok_json, Tool, ToolContext};
use crate::adapters::quotes_commands::StockQuoteDto;
use crate::domain::agent::types::{PipelineKind, ToolResultContent};
use crate::domain::agent::ProviderKind;
use crate::domain::quotes::types::KlinePoint;
use crate::domain::quotes::KlinePeriod;
use crate::infrastructure::quotes::cache::kline_cache::{self, Category};
use crate::infrastructure::quotes::chart_renderer::{
    klinerow_to_point, render_kline_png, ChartRenderOptions,
};
use crate::infrastructure::quotes::snapshot::market_snapshot;
use crate::pipeline::agent::config::read_agent_config;
use crate::pipeline::market::overview as market_overview;
use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{json, Value};
use tauri::AppHandle;

/// 当前 chat channel 是否支持 tool_result 含 Image block？
/// - Anthropic：原生支持，agent 能看到图
/// - OpenAI Chat Completions：不支持，provider 把 Image 降级为 `"[image omitted]"`
/// - OpenAI Responses：tool 消息里也不支持 Image
///
/// get_kline 在 chart 模式前调一下这个——非 Anthropic 时自动 fallback 到 data 模式，
/// 避免给非 Anthropic 用户 agent 看不到图却以为看到了的 bug。
fn current_chat_supports_vision_in_tool_result(app: &AppHandle) -> bool {
    let cfg = read_agent_config(app);
    match cfg.resolve_pipeline(PipelineKind::Chat) {
        Ok((channel, _)) => matches!(channel.wire_format, ProviderKind::Anthropic),
        Err(_) => false, // 解析失败保守降级
    }
}

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
        "A 股实时行情快照（价 / 涨跌幅 / 成交量 / OHLC）。"
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
        let ts_code = match crate::infrastructure::quotes::repository::resolve_stock_ts_code(
            &self.app, &code,
        ) {
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
        "K 线。默认 mode=chart 返 PNG（蜡烛 + MA20 + 量，红涨绿跌），最适合判断趋势 / 形态。\
        需要精确数值传 mode=data 拿 OHLC 表；mode=both 两者都返。\
        chart/both 仅 Anthropic 渠道有效，OpenAI 渠道自动降级 data。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code":   { "type": "string", "description": "6 位 A 股代码或股票中文名" },
                "period": { "type": "string", "enum": ["day", "week", "month"], "default": "day" },
                "limit":  {
                    "type": "integer",
                    "minimum": 30,
                    "maximum": 800,
                    "default": 120,
                    "description": "K 线根数。chart 模式建议 60-120 根；data 模式按需"
                },
                "mode": {
                    "type": "string",
                    "enum": ["chart", "data", "both"],
                    "default": "chart",
                    "description": "chart=渲图给你看（默认，省 token，最直观）；data=OHLC 表（精确数值用）；both=两者都返"
                }
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
        let requested_mode = input
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("chart");
        // chart 模式仅 Anthropic 渠道有效——OpenAI Chat / Responses 在 tool_result
        // 里不支持 Image block，给它返图就是给 agent 一个"[image omitted]"占位，
        // agent 看不到图却以为能看到，会出乱判断。非 Anthropic 时静默降级到 data。
        let mode = if requested_mode == "chart" || requested_mode == "both" {
            if current_chat_supports_vision_in_tool_result(&self.app) {
                requested_mode
            } else {
                tracing::debug!(
                    requested_mode,
                    "get_kline: 当前渠道 tool_result 不支持 Image，降级到 data 模式"
                );
                "data"
            }
        } else {
            requested_mode
        };
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

        match mode {
            "data" => kline_data_payload(&code, period, &rows),
            "both" => kline_both_payload(&code, period, &rows),
            _ => kline_chart_payload(&code, period, &rows), // chart / 任何无效值 fallback
        }
    }
}

/// 渲染 PNG + 一段元数据文本（最近 close / MA20 / 区间 / 最近 5 根简表）。
/// 一次调用约 1500 token（图本身是 vision tokenizer 固定 ~1500），比纯 data 模式
/// 8-12k 省 80%+。
fn kline_chart_payload(
    code: &str,
    period: KlinePeriod,
    rows: &[kline_cache::KlineRow],
) -> (Vec<ToolResultContent>, bool) {
    let points: Vec<KlinePoint> = rows.iter().filter_map(klinerow_to_point).collect();
    if points.len() < 5 {
        return err_text(format!("{code} K 线数据不足 5 根，无法渲染"));
    }
    let title = format!("{} {} ({} bars)", code, period_label(period), points.len());
    let png = match render_kline_png(
        &points,
        &ChartRenderOptions {
            title,
            ..Default::default()
        },
    ) {
        Ok(b) => b,
        Err(e) => return err_text(format!("渲染失败：{e}")),
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    let summary = format_chart_summary(code, period, &points);
    let blocks = vec![
        ToolResultContent::Text { text: summary },
        ToolResultContent::Image {
            mime: "image/png".into(),
            data: b64,
        },
    ];
    (blocks, false)
}

/// 数值表模式——保留兼容性。返回完整 OHLC JSON。
fn kline_data_payload(
    code: &str,
    period: KlinePeriod,
    rows: &[kline_cache::KlineRow],
) -> (Vec<ToolResultContent>, bool) {
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
    (
        ok_json(json!({
            "code": code,
            "period": period_label(period),
            "count": klines.len(),
            "klines": klines,
        })),
        false,
    )
}

/// 图 + 简表（最近 20 根）。
fn kline_both_payload(
    code: &str,
    period: KlinePeriod,
    rows: &[kline_cache::KlineRow],
) -> (Vec<ToolResultContent>, bool) {
    let (mut chart_blocks, _) = kline_chart_payload(code, period, rows);
    let tail = rows.iter().rev().take(20).rev();
    let recent: Vec<Value> = tail
        .map(|r| {
            json!({
                "date": format_iso(&r.date),
                "open": r.open,
                "close": r.close,
                "high": r.high,
                "low": r.low,
                "volume": r.volume,
            })
        })
        .collect();
    let table_block = ToolResultContent::Text {
        text: format!("最近 20 根 OHLC：\n{}", serde_json::to_string(&recent).unwrap_or_default()),
    };
    chart_blocks.push(table_block);
    (chart_blocks, false)
}

/// 给 chart 模式生成一段"关键数值"摘要——agent 看图同时拿到精确数字定位。
fn format_chart_summary(code: &str, period: KlinePeriod, points: &[KlinePoint]) -> String {
    let first = points.first().unwrap();
    let last = points.last().unwrap();
    let close = last.close.value();
    let range_low = points
        .iter()
        .map(|p| p.low.value())
        .fold(f64::INFINITY, f64::min);
    let range_high = points
        .iter()
        .map(|p| p.high.value())
        .fold(f64::NEG_INFINITY, f64::max);
    let ma20 = if points.len() >= 20 {
        let sum: f64 = points[points.len() - 20..]
            .iter()
            .map(|p| p.close.value())
            .sum();
        Some(sum / 20.0)
    } else {
        None
    };
    let change_pct = if first.close.value() > 0.0 {
        (close - first.close.value()) / first.close.value() * 100.0
    } else {
        0.0
    };
    let ma20_line = match ma20 {
        Some(v) => format!("MA20 ¥{:.2}（最新价相对 MA20 {:+.2}%）", v, (close - v) / v * 100.0),
        None => "MA20 N/A（数据不足 20 根）".into(),
    };
    format!(
        "{} {} {} 根；区间 {} → {}，close ¥{:.2}（区间变化 {:+.2}%）\n\
         区间高低 ¥{:.2} / ¥{:.2}；{}\n\
         看图判断趋势 / 形态 / 位置。需要精确数值可调 mode=data。",
        code,
        period_label(period),
        points.len(),
        first.date.to_compact(),
        last.date.to_compact(),
        close,
        change_pct,
        range_high,
        range_low,
        ma20_line,
    )
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
        "大盘指数快照（上证 / 深证 / 创业板 / 科创 50）。"
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
