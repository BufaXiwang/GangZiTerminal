//! Expectation 写工具——create_expectation / update_expectation / cancel_expectation。
//!
//! Expectation 是 v3 投资决策的核心实体——agent 主动建仓必须先 create_expectation，
//! 然后再 open_position 关联 current_expectation_id。

use crate::domain::account::expectation::{
    Direction, Expectation, ExpectationEvent, ExpectationId, ExpectationState,
};
use crate::domain::account::thesis::Conviction;
use crate::domain::agent::types::ToolResultContent;
use crate::domain::shared::signal::SignalKind;
use crate::domain::shared::{OccurredAt, StockCode, Yuan};
use crate::infrastructure::account::expectation_repo;
use crate::pipeline::agent::tools::{err_text, ok_json, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::AppHandle;

fn parse_required_string(input: &Value, field: &str) -> Result<String, String> {
    input
        .get(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("缺少必填字段：{field}"))
}

fn parse_optional_string(input: &Value, field: &str) -> Option<String> {
    input.get(field).and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string())
}

fn parse_signals(input: &Value) -> Result<Vec<SignalKind>, String> {
    let arr = input
        .get("signals_used")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "signals_used 必须是 SignalKind 数组".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let s: SignalKind = serde_json::from_value(item.clone())
            .map_err(|e| format!("反序列化 signal 失败：{e}"))?;
        out.push(s);
    }
    Ok(out)
}

// ====== create_expectation ==============================================

pub struct CreateExpectationTool {
    app: AppHandle,
}

impl CreateExpectationTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for CreateExpectationTool {
    fn name(&self) -> &'static str {
        "create_expectation"
    }

    fn description(&self) -> &'static str {
        "创建一个 investment expectation——可量化、有时间窗口的押注。\
        必填：code（A 股 6 位代码），direction（up/down/range_bound），target_price，\
        horizon_days（交易日，相对当前），reasoning（叙事），signals_used（触发用的结构化信号数组），\
        conviction（low/medium/high）。\
        可选：target_price_ceiling（区间预期上沿）、theme（主题 tag）、supersedes_expectation_id（替换上一条）。\
        返回 expectation_id。后续如要开仓，用此 id 传给 open_position 的 expectation_id 字段。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {"type": "string"},
                "direction": {"type": "string", "enum": ["up", "down", "range_bound"]},
                "target_price": {"type": "number"},
                "target_price_ceiling": {"type": "number"},
                "horizon_days": {"type": "integer", "minimum": 1},
                "reasoning": {"type": "string"},
                "signals_used": {"type": "array"},
                "conviction": {"type": "string", "enum": ["low", "medium", "high"]},
                "theme": {"type": "string"},
                "supersedes_expectation_id": {"type": "string"}
            },
            "required": ["code", "direction", "horizon_days", "reasoning", "signals_used", "conviction"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let code_str = match parse_required_string(&input, "code") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let code = match StockCode::new(&code_str) {
            Ok(c) => c,
            Err(e) => return err_text(format!("非法 code {code_str}: {e:?}")),
        };
        let direction_str = match parse_required_string(&input, "direction") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let direction = match Direction::parse(&direction_str) {
            Some(d) => d,
            None => return err_text(format!("非法 direction: {direction_str}")),
        };
        let horizon_days = match input.get("horizon_days").and_then(|v| v.as_u64()) {
            Some(n) if n >= 1 => n as u32,
            _ => return err_text("horizon_days 必填且 ≥1"),
        };
        let reasoning = match parse_required_string(&input, "reasoning") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let signals_used = match parse_signals(&input) {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let conviction_str = match parse_required_string(&input, "conviction") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let conviction = match Conviction::parse(&conviction_str) {
            Some(c) => c,
            None => return err_text(format!("非法 conviction: {conviction_str}")),
        };
        let target_price = input.get("target_price").and_then(|v| v.as_f64()).map(Yuan::from_unchecked);
        let target_price_ceiling = input.get("target_price_ceiling").and_then(|v| v.as_f64()).map(Yuan::from_unchecked);
        let theme = parse_optional_string(&input, "theme");
        let supersedes = parse_optional_string(&input, "supersedes_expectation_id")
            .map(ExpectationId::from_string);

        let now = OccurredAt::now();
        // expires_at = now + horizon_days 自然日（Phase 1 简化，不接交易日历）
        let expires_at = OccurredAt::new(now.value() + (horizon_days as i64) * 24 * 3600 * 1000);
        let exp = Expectation::create(
            code,
            direction,
            target_price,
            target_price_ceiling,
            horizon_days,
            reasoning,
            signals_used,
            conviction,
            theme,
            supersedes,
            None, // regime_at_creation：W24 wire regime detector 时填
            now,
            expires_at,
        );
        if let Err(e) = expectation_repo::create(&self.app, &exp) {
            return err_text(format!("创建 expectation 失败：{e}"));
        }
        (
            ok_json(json!({
                "ok": true,
                "expectation_id": exp.id.as_str(),
                "expires_at_ms": exp.expires_at.value(),
            })),
            false,
        )
    }
}

// ====== update_expectation ==============================================

pub struct UpdateExpectationTool {
    app: AppHandle,
}

impl UpdateExpectationTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for UpdateExpectationTool {
    fn name(&self) -> &'static str {
        "update_expectation"
    }

    fn description(&self) -> &'static str {
        "调整一个 pending expectation 的 target_price / horizon / reasoning。\
        仅对 state=pending 有效。不能改 direction（要改方向应 cancel 后建新的）。\
        必填：expectation_id。可选：target_price, target_price_ceiling, horizon_days, reasoning。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "expectation_id": {"type": "string"},
                "target_price": {"type": "number"},
                "target_price_ceiling": {"type": "number"},
                "horizon_days": {"type": "integer", "minimum": 1},
                "reasoning": {"type": "string"}
            },
            "required": ["expectation_id"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let id_str = match parse_required_string(&input, "expectation_id") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let id = ExpectationId::from_string(id_str);
        let target_price = input.get("target_price").and_then(|v| v.as_f64()).map(Yuan::from_unchecked);
        let target_price_ceiling = input.get("target_price_ceiling").and_then(|v| v.as_f64()).map(Yuan::from_unchecked);
        let horizon_days = input.get("horizon_days").and_then(|v| v.as_u64()).map(|n| n as u32);
        let reasoning = parse_optional_string(&input, "reasoning");
        let new_expires = horizon_days.map(|d| {
            OccurredAt::new(OccurredAt::now().value() + (d as i64) * 24 * 3600 * 1000)
        });
        if let Err(e) = expectation_repo::update_fields(
            &self.app,
            &id,
            target_price,
            target_price_ceiling,
            horizon_days,
            new_expires,
            reasoning,
        ) {
            return err_text(format!("更新 expectation 失败：{e}"));
        }
        (ok_json(json!({"ok": true, "expectation_id": id.as_str()})), false)
    }
}

// ====== cancel_expectation ==============================================

pub struct CancelExpectationTool {
    app: AppHandle,
}

impl CancelExpectationTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for CancelExpectationTool {
    fn name(&self) -> &'static str {
        "cancel_expectation"
    }

    fn description(&self) -> &'static str {
        "主动取消一个 pending expectation——区别于到期 missed/expired。\
        用于 agent 判断假设已经不成立主动撤。必填：expectation_id, reason。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "expectation_id": {"type": "string"},
                "reason": {"type": "string"}
            },
            "required": ["expectation_id", "reason"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let id_str = match parse_required_string(&input, "expectation_id") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let reason = match parse_required_string(&input, "reason") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let id = ExpectationId::from_string(id_str);
        let now = OccurredAt::now();
        let event = ExpectationEvent::Cancelled {
            reason: reason.clone(),
        };
        if let Err(e) = expectation_repo::transition(
            &self.app,
            &id,
            ExpectationState::Cancelled,
            event,
            now,
        ) {
            return err_text(format!("取消 expectation 失败：{e}"));
        }
        (ok_json(json!({"ok": true, "expectation_id": id.as_str(), "state": "cancelled"})), false)
    }
}
