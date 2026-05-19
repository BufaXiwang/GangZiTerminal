//! 模拟账户写工具 + 账户读工具——chat 模式下 agent mid-loop 调用。
//!
//! 5 个工具：
//! - `get_account`：查账户快照（现金 / 总盈亏 / 持仓明细）
//! - `open_position`：开新仓
//! - `close_position`：全平
//! - `scale_position`:加 / 减仓
//! - `adjust_stops`：调止损 / 止盈 / 时间止损
//!
//! 写操作全部走 `pipeline::account::AccountService`——唯一写入口，含 mutex、规则校验、
//! 事件 + state 同事务落盘。失败（涨跌停 / T+1 / 资金不足等）以 is_error=true 返给 agent，
//! 让 agent 在下一轮看到错误并向用户解释。
//!
//! agent 调用 source 统一传 `EventSource::Chat { message_id }`——run_id 充当 message_id，
//! 让后续 review/审计能追溯到具体 chat run。

use crate::pipeline::agent::tools::{err_text, ok_json, Tool, ToolContext};
use crate::domain::account::types::{CloseReason, EventSource, Position};
use crate::domain::agent::types::ToolResultContent;
use crate::domain::shared::{OccurredAt, Shares, Yuan};
use crate::pipeline::account::service::{AccountService, OpenRequest};
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
        "获取模拟账户当前快照：现金、市值、已实现盈亏、未实现盈亏、所有 open 持仓明细。\
        决定开/加/减/平仓前必查，避免凭印象。"
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
        "在模拟账户开新仓（A 股，整数手 = 100 股一手）。入场价由后端按最新实时报价决定，\
        agent 只提供数量 + 思路 + 可选止损止盈。\
        \n失败常见原因：涨/跌停（买不进/卖不出）、T+1（当日开仓不可减）、资金不足、\
        重复开仓（同一 code 已有 open）、code 不存在、盘外（仅交易时段允许开）。\
        失败会返 is_error=true，请如实告诉用户。\
        \n**必填**：code（6 位代码或可解析名）、shares（100 的整数倍）、thesis（开仓理由摘要）、\
        expectation_id（先 `create_expectation` 拿到的预期 id——agent 通过 chat 主动开仓**必须**\
        关联预期；否则该笔交易无法在 reflection 阶段被复盘）。\
        \n建议同时提供 stop_loss / take_profit，让账户能在价格突破时给出 reason。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "A 股 6 位代码或可解析的中文名" },
                "shares": { "type": "integer", "description": "股数，必须 100 的整数倍（最小 1 手）" },
                "thesis": { "type": "string", "description": "开仓备注（≤120 字，UI 快速展示用；结构化的为什么写在 Expectation 里）" },
                "expectation_id": { "type": "string", "description": "关联的 Expectation id——必须先 create_expectation 拿到再开仓，让 reflection 能复盘" },
                "stop_loss": { "type": "number", "description": "止损价（可选，元）" },
                "take_profit": { "type": "number", "description": "止盈价（可选，元）" },
                "name": { "type": "string", "description": "公司名（可省略，会自动从行情拉取）" },
                "note": { "type": "string", "description": "agent 备注（markdown，可省略）" }
            },
            "required": ["code", "shares", "thesis", "expectation_id"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let code = match crate::pipeline::stocks::resolve_stock(
            &self.app,
            input
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim(),
        )
        .await
        {
            Ok(stock) => stock.code,
            Err(e) => return err_text(format!("code 解析失败：{e}")),
        };
        let shares_n = match parse_required_shares(&input, "shares") {
            Ok(n) => n,
            Err(e) => return err_text(e),
        };
        let thesis = match parse_required_string(&input, "thesis") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let stop_loss = parse_optional_yuan(&input, "stop_loss");
        let take_profit = parse_optional_yuan(&input, "take_profit");
        let name = optional_string(&input, "name");
        let note = optional_string(&input, "note");

        // expectation_id 必填——agent 主动开仓必须有可复盘的预期，否则 reflection 阶段无对照。
        // 用户手动开仓走前端 IPC（account_commands.rs），那条路径仍允许 None。
        let expectation_id_raw = optional_string(&input, "expectation_id");
        if expectation_id_raw.is_empty() {
            return err_text(
                "expectation_id 必填——请先调 create_expectation 创建预期再来开仓".to_string(),
            );
        }
        let expectation_id = Some(
            crate::domain::account::expectation::ExpectationId::from_string(expectation_id_raw),
        );

        let req = OpenRequest {
            code,
            shares: Shares::from_unchecked(shares_n),
            name,
            thesis,
            expectation_id,
            stop_loss,
            take_profit,
            time_stop_at: None, // 留空让 service 自动算 entered_at + 7 日历日
            source: chat_event_source(ctx),
            source_analysis_id: String::new(),
            agent_note_md: note,
        };

        let service = AccountService::new(self.app.clone());
        match service.open_position(req).await {
            Ok(position) => (ok_json(position_to_json(&position)), false),
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
        "在模拟账户全平一个 open 持仓。需要 position_id（从 get_account 拿）。\
        \n失败常见原因：T+1（当日开仓不可平）、跌停（卖不出去）、盘外、position_id 不存在。\
        \nreason 用于复盘归因，填 manual / stop_loss / take_profit / time_stop / invalidated 之一。"
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
        "加仓或减仓一个 open 持仓。shares_delta 正数 = 加仓，负数 = 减仓。\
        \n加仓后均价按加权平均更新；减仓不动均价。\
        减仓全清会拒绝——想全清请用 close_position。\
        \n失败常见原因：T+1（减仓限制）、资金不足（加仓）、整手限制（结果须为 100 倍数）、\
        涨/跌停、盘外。"
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

// ===== adjust_stops =======================================================

pub struct AdjustStopsTool {
    app: AppHandle,
}

impl AdjustStopsTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for AdjustStopsTool {
    fn name(&self) -> &'static str {
        "adjust_stops"
    }

    fn description(&self) -> &'static str {
        "调整 open 持仓的止损 / 止盈 / 时间止损。\
        每个字段独立可选；不传 = 不改。\
        time_stop_at_ms 用 Unix ms 时间戳；不传 = 不动时间止损。\
        允许在盘外调（与 open/close/scale 不同——后者要求交易时段）。"
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
                    "description": "新时间止损（Unix 毫秒时间戳）；不传 = 不改"
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
