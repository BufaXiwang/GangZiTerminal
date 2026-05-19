//! Expectation 写工具——create_expectation / update_expectation / cancel_expectation。
//!
//! Expectation 是 v3 投资决策的核心实体——agent 主动建仓必须先 create_expectation，
//! 然后再 open_position 关联 current_expectation_id。

use crate::domain::account::expectation::{
    Conviction, Direction, Expectation, ExpectationEvent, ExpectationId, ExpectationState,
};
use crate::domain::agent::types::ToolResultContent;
use crate::domain::shared::signal::SignalKind;
use crate::domain::shared::{OccurredAt, StockCode, Yuan};
use crate::domain::agent::heuristic::HeuristicId;
use crate::infrastructure::account::expectation_repo;
use crate::infrastructure::agent::expectation_heuristic_link_repo;
use crate::infrastructure::quotes::snapshot::market_snapshot;
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

/// 解析可选 `invalidation_signals` 数组——缺失或非数组返回空 Vec（不视为错误）。
fn parse_invalidation_signals(input: &Value) -> Result<Vec<SignalKind>, String> {
    let Some(arr) = input.get("invalidation_signals").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let s: SignalKind = serde_json::from_value(item.clone())
            .map_err(|e| format!("反序列化 invalidation signal 失败：{e}"))?;
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
        "创建可量化 expectation——agent 主动开仓前必先调拿 id 传给 open_position。\
        返回 expectation_id。supersedes_expectation_id 用于替换同方向旧预期。\
        reference_price 不传时自动取当前市价快照——用于到期判定 partial_hit。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {"type": "string"},
                "direction": {"type": "string", "enum": ["up", "down", "range_bound"]},
                "target_price": {"type": "number"},
                "target_price_ceiling": {"type": "number"},
                "reference_price": {"type": "number", "description": "决策当下的参考价；省略时取 market_snapshot 当前价"},
                "horizon_days": {"type": "integer", "minimum": 1},
                "reasoning": {"type": "string"},
                "signals_used": {"type": "array"},
                "invalidation_signals": {
                    "type": "array",
                    "description": "失效条件信号数组：review 时若任一 family 命中，提前判 Missed（不等 horizon）。例如 Up 预期可填 BreakoutBelow20MA / VolumeShrink / LimitDown。"
                },
                "conviction": {"type": "string", "enum": ["low", "medium", "high"]},
                "theme": {"type": "string"},
                "supersedes_expectation_id": {"type": "string"},
                "applied_heuristic_ids": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "本预期实际依赖的 heuristic id 列表（按 system prompt 给出的 id 复制）；review 时按此精确给对应 heuristic 计 hit/miss。不依赖任何 heuristic 就别填。"
                }
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
        let invalidation_signals = match parse_invalidation_signals(&input) {
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
        // reference_price：用户传 > snapshot 当前价 > None。
        let reference_price = input
            .get("reference_price")
            .and_then(|v| v.as_f64())
            .map(Yuan::from_unchecked)
            .or_else(|| {
                market_snapshot::get(&code.to_ts_code())
                    .and_then(|q| q.price)
            });
        let theme = parse_optional_string(&input, "theme");
        let supersedes = parse_optional_string(&input, "supersedes_expectation_id")
            .map(ExpectationId::from_string);
        let applied_heuristic_ids: Vec<HeuristicId> = input
            .get("applied_heuristic_ids")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| HeuristicId::from_string(s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let now = OccurredAt::now();
        // expires_at = now + horizon_days 自然日（Phase 1 简化，不接交易日历）
        let expires_at = OccurredAt::new(now.value() + (horizon_days as i64) * 24 * 3600 * 1000);
        let exp = Expectation::create(
            code,
            direction,
            target_price,
            target_price_ceiling,
            reference_price,
            horizon_days,
            reasoning,
            signals_used,
            invalidation_signals,
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
        let linked_count = applied_heuristic_ids.len();
        if !applied_heuristic_ids.is_empty() {
            if let Err(e) =
                expectation_heuristic_link_repo::record(&self.app, &exp.id, &applied_heuristic_ids)
            {
                // 不阻断主流程——expectation 已写入，link 失败只影响后续归因
                tracing::warn!(error = %e, expectation = %exp.id, "写 expectation_heuristic_links 失败");
            }
        }
        (
            ok_json(json!({
                "ok": true,
                "expectation_id": exp.id.as_str(),
                "expires_at_ms": exp.expires_at.value(),
                "linked_heuristics": linked_count,
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
        "调 pending expectation 的 target / horizon / reasoning。不能改 direction\
        （要改方向请先 cancel）。"
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
        "主动撤 pending expectation——区别于到期自动 missed/expired。\
        agent 判断假设已不成立时调。"
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
