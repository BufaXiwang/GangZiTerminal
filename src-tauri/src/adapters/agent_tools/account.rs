//! 模拟账户写工具 + 账户读工具——chat 模式下 agent mid-loop 调用。
//!
//! v4 工具（合并 Expectation 后）：
//! - `get_account`：查账户快照（现金 / 总盈亏 / 持仓明细）
//! - `open_position`：开新仓（含 direction / signals_used / invalidation_signals / reasoning 等假设字段）
//! - `close_position`：全平（agent 主观撤回；auto_review 会自动平触发条件命中的）
//! - `scale_position`：加 / 减仓（仅 Live）
//! - `adjust_position`：调 take_profit / stop_loss / time_stop / invalidation_signals / reasoning
//!
//! 写操作全部走 `pipeline::account::AccountService`——唯一写入口，含 mutex、规则校验、
//! 事件 + state 同事务落盘。失败（涨跌停 / T+1 / 资金不足等）以 is_error=true 返给 agent。

use crate::domain::account::position::{Direction, PositionKind};
use crate::domain::account::types::{CloseReason, EventSource, Position};
use crate::domain::agent::heuristic::HeuristicId;
use crate::domain::agent::types::ToolResultContent;
use crate::domain::shared::signal::SignalKind;
use crate::domain::shared::{OccurredAt, Shares, Yuan};
use crate::infrastructure::agent::position_heuristic_link_repo;
use crate::pipeline::account::service::{AccountService, OpenRequest};
use crate::pipeline::agent::tools::{err_text, ok_json, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::AppHandle;

// ===== 工具间共用 helper ==================================================

fn parse_position_id(input: &Value) -> Result<crate::domain::account::types::PositionId, String> {
    let id = input
        .get("position_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing position_id".to_string())?
        .trim();
    if id.is_empty() {
        return Err("position_id 为空".into());
    }
    Ok(crate::domain::account::types::PositionId::from_string(
        id.to_string(),
    ))
}

fn parse_required_shares(input: &Value, field: &str) -> Result<i64, String> {
    input
        .get(field)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("missing or invalid {field}（必须为整数股数）"))
}

fn parse_optional_yuan(input: &Value, field: &str) -> Option<Yuan> {
    input
        .get(field)
        .and_then(Value::as_f64)
        .map(Yuan::from_unchecked)
}

fn parse_required_string(input: &Value, field: &str) -> Result<String, String> {
    let s = input
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {field}"))?
        .trim();
    if s.is_empty() {
        Err(format!("{field} 为空"))
    } else {
        Ok(s.to_string())
    }
}

fn optional_string(input: &Value, field: &str) -> String {
    input
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn parse_close_reason(input: &Value) -> CloseReason {
    match input.get("reason").and_then(Value::as_str) {
        Some("stop_loss") => CloseReason::StopLoss,
        Some("take_profit") => CloseReason::TakeProfit,
        Some("time_stop") => CloseReason::TimeStop,
        Some("invalidated") => CloseReason::Invalidated,
        _ => CloseReason::Manual,
    }
}

fn parse_signals(input: &Value, field: &str) -> Result<Vec<SignalKind>, String> {
    let Some(arr) = input.get(field).and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let s: SignalKind = serde_json::from_value(item.clone())
            .map_err(|e| format!("反序列化 {field} 失败：{e}"))?;
        out.push(s);
    }
    Ok(out)
}

fn position_to_json(p: &Position) -> Value {
    serde_json::to_value(p).unwrap_or(Value::Null)
}

fn chat_event_source(ctx: &ToolContext) -> EventSource {
    EventSource::Chat {
        message_id: ctx.run_id.clone(),
    }
}

// ===== get_account ========================================================

pub struct GetAccountTool {
    app: AppHandle,
}

impl GetAccountTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for GetAccountTool {
    fn name(&self) -> &'static str {
        "get_account"
    }

    fn description(&self) -> &'static str {
        "账户快照：现金 / 市值 / PnL / open 持仓（含 live + watch）。开/平/调仓前必查。"
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn execute(&self, _input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let service = AccountService::new(self.app.clone());
        match service.snapshot() {
            Ok(snap) => {
                let value =
                    serde_json::to_value(&snap).unwrap_or_else(|_| json!({"error": "序列化失败"}));
                (ok_json(value), false)
            }
            Err(e) => err_text(format!("读账户快照失败：{e}")),
        }
    }
}

// ===== open_position ======================================================

pub struct OpenPositionTool {
    app: AppHandle,
}

impl OpenPositionTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for OpenPositionTool {
    fn name(&self) -> &'static str {
        "open_position"
    }

    fn description(&self) -> &'static str {
        "开新仓（A 股 100 股整数倍）+ 声明投资假设。\
        kind=live 真持仓扣现金；kind=watch 观察型不动现金但走完整学习闭环。\
        direction / take_profit / stop_loss / invalidation_signals 是触发条件——\
        scheduler tick 命中后自动平仓 + 写 lesson + 反向打标 heuristic。\
        reasoning 是自然语言决策上下文（无字数限制）。\
        失败原因看 is_error 文本。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "A 股 6 位代码或可解析的中文名" },
                "shares": { "type": "integer", "description": "股数，100 整数倍（kind=watch 时省略或填 0）" },
                "kind": { "type": "string", "enum": ["live", "watch"], "default": "live", "description": "live=真持仓；watch=看好但不下注（shares=0）" },
                "direction": { "type": "string", "enum": ["up", "down"], "description": "看涨 / 看跌——决定 take_profit / stop_loss 的语义" },
                "reasoning": { "type": "string", "description": "自然语言决策上下文（为什么押这一手），无字数限制" },
                "signals_used": { "type": "array", "description": "触发本次建仓的结构化 SignalKind 数组——close 时反向打标 heuristic" },
                "invalidation_signals": { "type": "array", "description": "失效条件 SignalKind 数组：scheduler 检测到任一 family 命中即提前判 Invalidated 平仓" },
                "take_profit": { "type": "number", "description": "目标止盈价（命中自动 close(TakeProfit)）" },
                "stop_loss": { "type": "number", "description": "价格止损（命中自动 close(StopLoss)）" },
                "time_stop_days": { "type": "integer", "description": "时间止损：N 个日历日后到期自动 close(TimeStop)；不传默认 7 天" },
                "name": { "type": "string", "description": "公司名（可省略，会自动拉取）" },
                "note": { "type": "string", "description": "agent 备注（markdown）" },
                "applied_heuristic_ids": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "本仓位实际依赖的 heuristic id 列表——close 时按此精确给对应 heuristic 计 hit/miss"
                }
            },
            "required": ["code", "direction", "reasoning"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let code = match crate::pipeline::stocks::resolve_stock(
            &self.app,
            input.get("code").and_then(Value::as_str).unwrap_or("").trim(),
        )
        .await
        {
            Ok(stock) => stock.code,
            Err(e) => return err_text(format!("code 解析失败：{e}")),
        };

        let kind_str = optional_string(&input, "kind");
        let kind = if kind_str.is_empty() {
            PositionKind::Live
        } else {
            match PositionKind::parse(&kind_str) {
                Some(k) => k,
                None => return err_text(format!("非法 kind: {kind_str}")),
            }
        };

        let direction_str = match parse_required_string(&input, "direction") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let direction = match Direction::parse(&direction_str) {
            Some(d) => d,
            None => return err_text(format!("非法 direction: {direction_str}")),
        };

        let shares_n = if matches!(kind, PositionKind::Watch) {
            // Watch 强制 0；忽略传入值
            0i64
        } else {
            match parse_required_shares(&input, "shares") {
                Ok(n) => n,
                Err(e) => return err_text(e),
            }
        };

        let reasoning = match parse_required_string(&input, "reasoning") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let signals_used = match parse_signals(&input, "signals_used") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let invalidation_signals = match parse_signals(&input, "invalidation_signals") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let stop_loss = parse_optional_yuan(&input, "stop_loss");
        let take_profit = parse_optional_yuan(&input, "take_profit");
        let time_stop_at = input
            .get("time_stop_days")
            .and_then(Value::as_i64)
            .map(|days| {
                OccurredAt::new(OccurredAt::now().value() + days * 24 * 3600 * 1000)
            });

        let name = optional_string(&input, "name");
        let note = optional_string(&input, "note");
        let applied_heuristic_ids: Vec<HeuristicId> = input
            .get("applied_heuristic_ids")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| HeuristicId::from_string(s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let req = OpenRequest {
            code,
            shares: Shares::from_unchecked(shares_n),
            name,
            kind,
            direction,
            reasoning,
            signals_used,
            invalidation_signals,
            stop_loss,
            take_profit,
            time_stop_at,
            source: chat_event_source(ctx),
            source_analysis_id: String::new(),
            agent_note_md: note,
        };

        let service = AccountService::new(self.app.clone());
        match service.open_position(req).await {
            Ok(position) => {
                if !applied_heuristic_ids.is_empty() {
                    if let Err(e) = position_heuristic_link_repo::record(
                        &self.app,
                        &position.id,
                        &applied_heuristic_ids,
                    ) {
                        tracing::warn!(error = %e, position = %position.id, "写 position_heuristic_links 失败");
                    }
                }
                (ok_json(position_to_json(&position)), false)
            }
            Err(e) => err_text(format!("开仓失败：{e}")),
        }
    }
}

// ===== close_position =====================================================

pub struct ClosePositionTool {
    app: AppHandle,
}

impl ClosePositionTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for ClosePositionTool {
    fn name(&self) -> &'static str {
        "close_position"
    }

    fn description(&self) -> &'static str {
        "全平 open 持仓。reason 填 manual/stop_loss/take_profit/time_stop/invalidated。\
        触发条件命中由 scheduler auto_review 自动平仓，agent 仅在主观撤回时调本工具（reason=manual）。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "position_id": { "type": "string", "description": "从 get_account 列表里的 id 字段" },
                "reason": {
                    "type": "string",
                    "enum": ["manual", "stop_loss", "take_profit", "time_stop", "invalidated"],
                    "description": "平仓归因，缺省 manual"
                },
                "note": { "type": "string", "description": "agent 备注（markdown）" }
            },
            "required": ["position_id"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let position_id = match parse_position_id(&input) {
            Ok(p) => p,
            Err(e) => return err_text(e),
        };
        let reason = parse_close_reason(&input);
        let note = optional_string(&input, "note");

        let service = AccountService::new(self.app.clone());
        match service
            .close_position(&position_id, reason, chat_event_source(ctx), note)
            .await
        {
            Ok(position) => (ok_json(position_to_json(&position)), false),
            Err(e) => err_text(format!("平仓失败：{e}")),
        }
    }
}

// ===== scale_position =====================================================

pub struct ScalePositionTool {
    app: AppHandle,
}

impl ScalePositionTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for ScalePositionTool {
    fn name(&self) -> &'static str {
        "scale_position"
    }

    fn description(&self) -> &'static str {
        "加减仓 open 持仓（仅 live）。shares_delta 正加负减（100 整数倍）。\
        全清用 close_position（本工具拒绝全清）。加仓后均价加权平均；减仓不动均价。\
        Watch 类型不允许 scale——想转 live 请先 close_position 再 open_position。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "position_id": { "type": "string" },
                "shares_delta": {
                    "type": "integer",
                    "description": "正=加仓，负=减仓；绝对值必须 100 整数倍"
                },
                "note": { "type": "string" }
            },
            "required": ["position_id", "shares_delta"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let position_id = match parse_position_id(&input) {
            Ok(p) => p,
            Err(e) => return err_text(e),
        };
        let shares_delta = match parse_required_shares(&input, "shares_delta") {
            Ok(n) => n,
            Err(e) => return err_text(e),
        };
        let note = optional_string(&input, "note");

        let service = AccountService::new(self.app.clone());
        match service
            .scale_position(&position_id, shares_delta, note, chat_event_source(ctx))
            .await
        {
            Ok(position) => (ok_json(position_to_json(&position)), false),
            Err(e) => err_text(format!("加减仓失败：{e}")),
        }
    }
}

// ===== adjust_position ====================================================

pub struct AdjustPositionTool {
    app: AppHandle,
}

impl AdjustPositionTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for AdjustPositionTool {
    fn name(&self) -> &'static str {
        "adjust_position"
    }

    fn description(&self) -> &'static str {
        "调止盈 / 止损 / 时间止损（替代旧 adjust_stops）。\
        各字段独立可选，不传则不改。允许盘外调。\
        v4 本工具只调 take_profit / stop_loss / time_stop_at——\
        改 invalidation_signals / reasoning 后续可加（目前未暴露）。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "position_id": { "type": "string" },
                "stop_loss": { "type": "number", "description": "新止损价；不传 = 不改" },
                "take_profit": { "type": "number", "description": "新止盈价；不传 = 不改" },
                "time_stop_at_ms": {
                    "type": "integer",
                    "description": "新时间止损（Unix 毫秒）；不传 = 不改"
                },
                "note": { "type": "string" }
            },
            "required": ["position_id"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let position_id = match parse_position_id(&input) {
            Ok(p) => p,
            Err(e) => return err_text(e),
        };
        let stop_loss = parse_optional_yuan(&input, "stop_loss");
        let take_profit = parse_optional_yuan(&input, "take_profit");
        let time_stop_at = input
            .get("time_stop_at_ms")
            .and_then(Value::as_i64)
            .map(OccurredAt::new);
        let note = optional_string(&input, "note");

        let service = AccountService::new(self.app.clone());
        match service
            .adjust_stops(
                &position_id,
                stop_loss,
                take_profit,
                time_stop_at,
                chat_event_source(ctx),
                note,
            )
            .await
        {
            Ok(position) => (ok_json(position_to_json(&position)), false),
            Err(e) => err_text(format!("调止损失败：{e}")),
        }
    }
}
