//! Thesis 写工具——create_thesis / update_thesis_state / attach_thesis_feedback。
//!
//! 这些工具让 agent 把"投资论点"作为一等公民显式管理：开仓前先 create_thesis、
//! reflection 时 update_thesis_state、用户反馈时 attach_thesis_feedback。
//!
//! 见 docs/design/agent-redesign.md 关键概念：Thesis。

use crate::domain::account::thesis::{
    Conviction, Thesis, ThesisEvent, ThesisId, ThesisState,
};
use crate::domain::agent::types::ToolResultContent;
use crate::domain::shared::{OccurredAt, StockCode};
use crate::infrastructure::account::thesis_repo;
use crate::pipeline::agent::tools::{err_text, ok_json, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::AppHandle;

// ====== helpers =========================================================

fn parse_required_string(input: &Value, field: &str) -> Result<String, String> {
    input
        .get(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("缺少必填字段：{field}"))
}

fn parse_string_array(input: &Value, field: &str) -> Vec<String> {
    input
        .get(field)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn parse_conviction(input: &Value) -> Result<Conviction, String> {
    let raw = parse_required_string(input, "conviction")?;
    match raw.as_str() {
        "low" => Ok(Conviction::Low),
        "medium" => Ok(Conviction::Medium),
        "high" => Ok(Conviction::High),
        other => Err(format!("conviction 必须是 low/medium/high，收到：{other}")),
    }
}

fn parse_codes(input: &Value) -> Result<Vec<StockCode>, String> {
    let raw = parse_string_array(input, "target_codes");
    let mut out = Vec::with_capacity(raw.len());
    for s in raw {
        let code = StockCode::new(&s).map_err(|e| format!("非法 StockCode {s}: {e:?}"))?;
        out.push(code);
    }
    Ok(out)
}

// ====== create_thesis ====================================================

pub struct CreateThesisTool {
    app: AppHandle,
}

impl CreateThesisTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for CreateThesisTool {
    fn name(&self) -> &'static str {
        "create_thesis"
    }

    fn description(&self) -> &'static str {
        "创建一个投资论点（投资逻辑）作为独立实体——不一定立即开仓，可以纯跟踪。\
        必填：hypothesis（核心论点）、invalidation（什么发生就证伪）、conviction（low/medium/high）。\
        强烈建议：validation_checks（验证清单 ≥2 条）、target_codes（涉及的 6 位股票代码列表）。\
        默认 state=active（立即跟踪）。返回 thesis_id 用于后续 open_position 关联。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "hypothesis": {"type": "string", "description": "核心论点：你赌的是什么发生"},
                "invalidation": {"type": "string", "description": "失效条件：什么事一出现就证伪"},
                "validation_checks": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "验证清单：盯哪些指标 / 事件能确认论点在兑现"
                },
                "conviction": {"type": "string", "enum": ["low", "medium", "high"]},
                "target_codes": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "涉及的 A 股 6 位代码（可多个，一个 thesis 可分仓多只）"
                },
                "as_draft": {
                    "type": "boolean",
                    "description": "true=state=drafted（起草中不影响决策）；默认 false=active"
                }
            },
            "required": ["hypothesis", "invalidation", "conviction"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let hypothesis = match parse_required_string(&input, "hypothesis") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let invalidation = match parse_required_string(&input, "invalidation") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let conviction = match parse_conviction(&input) {
            Ok(c) => c,
            Err(e) => return err_text(e),
        };
        let validation_checks = parse_string_array(&input, "validation_checks");
        let codes = match parse_codes(&input) {
            Ok(c) => c,
            Err(e) => return err_text(e),
        };
        let as_draft = input
            .get("as_draft")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let now = OccurredAt::now();
        let thesis = if as_draft {
            Thesis::draft(
                hypothesis,
                invalidation,
                validation_checks,
                conviction,
                codes,
                None,
                now,
            )
        } else {
            Thesis::active(
                hypothesis,
                invalidation,
                validation_checks,
                conviction,
                codes,
                None,
                now,
            )
        };
        if let Err(e) = thesis_repo::create_thesis(&self.app, &thesis) {
            return err_text(format!("创建 thesis 失败：{e}"));
        }
        (
            ok_json(json!({
                "ok": true,
                "thesis_id": thesis.id.as_str(),
                "state": thesis.state.as_str(),
                "target_codes": thesis
                    .target_codes
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>(),
            })),
            false,
        )
    }
}

// ====== update_thesis_state =============================================

pub struct UpdateThesisStateTool {
    app: AppHandle,
}

impl UpdateThesisStateTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for UpdateThesisStateTool {
    fn name(&self) -> &'static str {
        "update_thesis_state"
    }

    fn description(&self) -> &'static str {
        "把 thesis 的状态推进到下一个态：active / validated / drifted / invalidated / abandoned。\
        必填：thesis_id, new_state, reason（为什么转这个态，至少一句话）。\
        进入终态（validated/drifted/invalidated/abandoned）会自动落 closed_at。\
        用法：reflection 时根据 invalidation/validation 对照结果调；用户反馈推翻时也调。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "thesis_id": {"type": "string"},
                "new_state": {
                    "type": "string",
                    "enum": ["active", "validated", "drifted", "invalidated", "abandoned"]
                },
                "reason": {"type": "string", "description": "状态推进的理由"}
            },
            "required": ["thesis_id", "new_state", "reason"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let id_raw = match parse_required_string(&input, "thesis_id") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let new_state_raw = match parse_required_string(&input, "new_state") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let reason = match parse_required_string(&input, "reason") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let id = ThesisId::from_string(id_raw);
        let new_state = match new_state_raw.as_str() {
            "active" => ThesisState::Active,
            "validated" => ThesisState::Validated,
            "drifted" => ThesisState::Drifted,
            "invalidated" => ThesisState::Invalidated,
            "abandoned" => ThesisState::Abandoned,
            other => return err_text(format!("非法 new_state: {other}")),
        };
        let event = match new_state {
            ThesisState::Active => ThesisEvent::Activated,
            ThesisState::Validated => ThesisEvent::Validated { reason: reason.clone() },
            ThesisState::Drifted => ThesisEvent::Drifted { reason: reason.clone() },
            ThesisState::Invalidated => {
                ThesisEvent::Invalidated { reason: reason.clone() }
            }
            ThesisState::Abandoned => ThesisEvent::Abandoned { reason: reason.clone() },
            _ => unreachable!(),
        };
        let now = OccurredAt::now();
        if let Err(e) = thesis_repo::update_thesis_state(&self.app, &id, new_state, event, now) {
            return err_text(format!("更新 thesis state 失败：{e}"));
        }
        (
            ok_json(json!({
                "ok": true,
                "thesis_id": id.as_str(),
                "new_state": new_state.as_str(),
                "reason": reason,
            })),
            false,
        )
    }
}

// ====== attach_thesis_feedback ==========================================

pub struct AttachThesisFeedbackTool {
    app: AppHandle,
}

impl AttachThesisFeedbackTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for AttachThesisFeedbackTool {
    fn name(&self) -> &'static str {
        "attach_thesis_feedback"
    }

    fn description(&self) -> &'static str {
        "把一段用户反馈追加到 thesis 的事件链——下次 reflection 时一并参考。\
        不立即变 principle（避免一句话被过度泛化）。\
        必填：thesis_id, text（用户反馈原文，最好包含「为什么」）。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "thesis_id": {"type": "string"},
                "text": {"type": "string"}
            },
            "required": ["thesis_id", "text"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let id_raw = match parse_required_string(&input, "thesis_id") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let text = match parse_required_string(&input, "text") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let id = ThesisId::from_string(id_raw);
        let event = ThesisEvent::UserFeedback { text: text.clone() };
        let now = OccurredAt::now();
        if let Err(e) = thesis_repo::append_thesis_event(&self.app, &id, event, now) {
            return err_text(format!("追加 thesis_event 失败：{e}"));
        }
        (
            ok_json(json!({
                "ok": true,
                "thesis_id": id.as_str(),
            })),
            false,
        )
    }
}
