//! 视觉信号工具——analyze_chart 渲 K 线 PNG 喂给 LLM vision；
//! propose_visual_pattern 让 LLM 把识到的形态落 SignalDetection。

use crate::domain::agent::types::ToolResultContent;
use crate::domain::quotes::types::KlinePoint;
use crate::domain::shared::signal::SignalKind;
use crate::domain::shared::{Lots, OccurredAt, StockCode, TradeDate, Yuan};
use crate::infrastructure::quotes::cache::kline_cache::{self, Category, KlineRow};
use crate::infrastructure::quotes::chart_renderer::{render_kline_png, ChartRenderOptions};
use crate::infrastructure::quotes::repository::resolve_stock_ts_code;
use crate::infrastructure::agent::signal_detection_repo;
use crate::pipeline::agent::tools::{err_text, ok_json, Tool, ToolContext};
use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{json, Value};
use tauri::AppHandle;

// ===== analyze_chart ====================================================

pub struct AnalyzeChartTool {
    app: AppHandle,
}

impl AnalyzeChartTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

fn klinerow_to_point(r: &KlineRow) -> Option<KlinePoint> {
    let date = TradeDate::from_compact(&r.date).ok()?;
    Some(KlinePoint {
        date,
        open: Yuan::from_unchecked(r.open),
        close: Yuan::from_unchecked(r.close),
        high: Yuan::from_unchecked(r.high),
        low: Yuan::from_unchecked(r.low),
        volume: Lots::from_unchecked(r.volume.unwrap_or(0.0) as i64),
        amount: Yuan::from_unchecked(r.amount.unwrap_or(0.0)),
    })
}

#[async_trait]
impl Tool for AnalyzeChartTool {
    fn name(&self) -> &'static str {
        "analyze_chart"
    }

    fn description(&self) -> &'static str {
        "把个股 K 线渲染成 PNG 给你看——用于识别经典形态（双底/头肩顶/突破/旗形/缠绕等）。\
        返回 image block + 简要元数据。看完后必须调 propose_visual_pattern 把识到的结构落库，\
        否则 vision 观察不会进入信号链。\
        参数：code（6 位 A 股代码），period（day/week，默认 day），lookback_days（默认 120）。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": { "type": "string" },
                "period": { "type": "string", "enum": ["day", "week"], "default": "day" },
                "lookback_days": { "type": "integer", "minimum": 30, "maximum": 240, "default": 120 }
            },
            "required": ["code"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let code_raw = match input.get("code").and_then(|v| v.as_str()) {
            Some(c) if !c.is_empty() => c.to_string(),
            _ => return err_text("缺少必填字段：code".to_string()),
        };
        let period = match input.get("period").and_then(|v| v.as_str()) {
            Some("week") => crate::domain::quotes::KlinePeriod::Week,
            _ => crate::domain::quotes::KlinePeriod::Day,
        };
        let limit = input
            .get("lookback_days")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(120)
            .clamp(30, 240);

        let ts_code = match resolve_stock_ts_code(&self.app, &code_raw) {
            Some(ts) => ts,
            None => return err_text(format!("stocks 档案找不到 {code_raw}")),
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
            Err(e) => return err_text(format!("拉 K 线失败：{e}")),
        };
        if rows.is_empty() {
            return err_text(format!("{code_raw} 无 K 线数据"));
        }
        let points: Vec<KlinePoint> = rows.iter().filter_map(klinerow_to_point).collect();
        if points.len() < 30 {
            return err_text(format!("{code_raw} K 线数据不足 30 根，无法识别形态"));
        }
        let title = format!("{code_raw} {} ({} bars)", period_label(period), points.len());
        let png = match render_kline_png(&points, &ChartRenderOptions { title, ..Default::default() }) {
            Ok(b) => b,
            Err(e) => return err_text(format!("渲染失败：{e}")),
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let last = points.last().unwrap();
        let first = points.first().unwrap();
        let meta = ToolResultContent::Text {
            text: format!(
                "{} {} 区间 {}→{}，close {:.2}→{:.2}。\
                 看图后调 propose_visual_pattern 落 SignalDetection。",
                code_raw,
                period_label(period),
                first.date.to_compact(),
                last.date.to_compact(),
                first.close.value(),
                last.close.value(),
            ),
        };
        let img = ToolResultContent::Image {
            mime: "image/png".into(),
            data: b64,
        };
        (vec![meta, img], false)
    }
}

fn period_label(p: crate::domain::quotes::KlinePeriod) -> &'static str {
    match p {
        crate::domain::quotes::KlinePeriod::Day => "day",
        crate::domain::quotes::KlinePeriod::Week => "week",
        crate::domain::quotes::KlinePeriod::Month => "month",
    }
}

// ===== propose_visual_pattern ===========================================

pub struct ProposeVisualPatternTool {
    app: AppHandle,
}

impl ProposeVisualPatternTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for ProposeVisualPatternTool {
    fn name(&self) -> &'static str {
        "propose_visual_pattern"
    }

    fn description(&self) -> &'static str {
        "把你从 analyze_chart 看到的形态作为视觉信号落库——会写入 signal_detections，\
        后续 create_expectation 可在 signals_used 里引用 VisualPatternRead。\
        参数：code（6 位 A 股），pattern（如 double_bottom / head_and_shoulders_top / breakout / \
        flag / wedge / exhaustion_top），confidence (0..1)，timeframe（day/week/60m）。\
        confidence < 0.5 的形态视为不确定，仍可落但下游 strategy 通常忽略。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": { "type": "string" },
                "pattern": { "type": "string" },
                "confidence": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
                "timeframe": { "type": "string", "default": "day" }
            },
            "required": ["code", "pattern", "confidence"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let code_str = match input.get("code").and_then(|v| v.as_str()) {
            Some(c) if !c.is_empty() => c.to_string(),
            _ => return err_text("缺少 code".to_string()),
        };
        if StockCode::new(&code_str).is_err() {
            return err_text(format!("非法 A 股代码：{code_str}"));
        }
        let pattern = match input.get("pattern").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => return err_text("缺少 pattern".to_string()),
        };
        let confidence = match input.get("confidence").and_then(|v| v.as_f64()) {
            Some(c) if (0.0..=1.0).contains(&c) => c as f32,
            _ => return err_text("confidence 必须是 [0, 1] 的浮点".to_string()),
        };
        let timeframe = input
            .get("timeframe")
            .and_then(|v| v.as_str())
            .unwrap_or("day")
            .to_string();

        let signal = SignalKind::VisualPatternRead {
            pattern: pattern.clone(),
            confidence,
            timeframe: timeframe.clone(),
        };
        let detected_at = OccurredAt::now();
        if let Err(e) = signal_detection_repo::record_batch(
            &self.app,
            &ctx.run_id,
            &[(code_str.clone(), signal.clone(), detected_at)],
        ) {
            return err_text(format!("写入 signal_detection 失败：{e}"));
        }
        (
            ok_json(json!({
                "ok": true,
                "code": code_str,
                "pattern": pattern,
                "confidence": confidence,
                "timeframe": timeframe,
                "tick_id": ctx.run_id,
                "note": "已落 signal_detections，create_expectation 可在 signals_used 里引用 VisualPatternRead。",
            })),
            false,
        )
    }
}
