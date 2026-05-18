//! Principle 写工具——propose_principle / confirm_principle / retire_principle。
//!
//! Principle 是带 state / origin / hit_count 的结构化投资原则 / 已知偏差。
//!
//! 业务规则（agent-redesign.md § 5.2）：
//! - origin=user_stated 由用户消息触发（chat agent run 检测到偏好/纠错 → 调 propose 时设 origin=user_stated）
//! - origin=agent_inferred 由 reflection 触发
//! - propose 默认 state=proposed；用户复述或 ≥3 hit 后才升 active
//! - 本工具不校验 origin/state 业务规则——调用方（agent prompt 引导 + 工具入参）负责

use crate::domain::agent::principle::{
    Principle, PrincipleCategory, PrincipleId, PrincipleOrigin, PrincipleState,
};
use crate::domain::agent::types::ToolResultContent;
use crate::domain::quotes::regime::Regime;
use crate::domain::shared::OccurredAt;
use crate::infrastructure::agent::principle_repo;
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

fn parse_category(input: &Value) -> Result<PrincipleCategory, String> {
    let raw = parse_required_string(input, "category")?;
    PrincipleCategory::parse(&raw)
        .ok_or_else(|| format!("category 必须是 principle/known_bias/risk_preference，收到：{raw}"))
}

fn parse_regime_tags(input: &Value) -> Vec<Regime> {
    input
        .get("regime_tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter_map(Regime::parse)
                .collect()
        })
        .unwrap_or_default()
}

// ====== propose_principle ===============================================

pub struct ProposePrincipleTool {
    app: AppHandle,
}

impl ProposePrincipleTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for ProposePrincipleTool {
    fn name(&self) -> &'static str {
        "propose_principle"
    }

    fn description(&self) -> &'static str {
        "把一条值得长期持有的判断写成 principle。\
        必填：body（≤120 字）、category（principle/known_bias/risk_preference）、\
        origin（user_stated=用户口头说的 / agent_inferred=你自己 reflection 学到的）。\
        可选：regime_tags（适用市场状态列表，bull/bear/choppy；空表示通用）。\
        默认 state：origin=user_stated 直接 active；origin=agent_inferred 需要 proposed→≥3 hit 升 active。\
        调用时机：用户表达偏好 / 纠错 → user_stated；reflection 从亏损/盈利归因提炼 → agent_inferred。\
        不要把一次性指令写成 principle，只写「可反复应用于未来场景」的判断。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "body": {"type": "string", "maxLength": 120},
                "category": {"type": "string", "enum": ["principle", "known_bias", "risk_preference"]},
                "origin": {"type": "string", "enum": ["user_stated", "agent_inferred"]},
                "regime_tags": {
                    "type": "array",
                    "items": {"type": "string", "enum": ["bull", "bear", "choppy"]}
                }
            },
            "required": ["body", "category", "origin"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let body = match parse_required_string(&input, "body") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let category = match parse_category(&input) {
            Ok(c) => c,
            Err(e) => return err_text(e),
        };
        let origin_raw = match parse_required_string(&input, "origin") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let origin = match PrincipleOrigin::parse(&origin_raw) {
            Some(o) => o,
            None => return err_text(format!("非法 origin: {origin_raw}")),
        };
        let regime_tags = parse_regime_tags(&input);
        let now = OccurredAt::now();
        let principle = match origin {
            PrincipleOrigin::UserStated => Principle::from_user(body, category, regime_tags, now),
            PrincipleOrigin::AgentInferred => {
                Principle::propose_by_agent(body, category, regime_tags, now)
            }
        };
        if let Err(e) = principle_repo::create_principle(&self.app, &principle) {
            return err_text(format!("创建 principle 失败：{e}"));
        }
        (
            ok_json(json!({
                "ok": true,
                "principle_id": principle.id.as_str(),
                "state": principle.state.as_str(),
                "origin": principle.origin.as_str(),
            })),
            false,
        )
    }
}

// ====== confirm_principle ===============================================

pub struct ConfirmPrincipleTool {
    app: AppHandle,
}

impl ConfirmPrincipleTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for ConfirmPrincipleTool {
    fn name(&self) -> &'static str {
        "confirm_principle"
    }

    fn description(&self) -> &'static str {
        "把一条 proposed 的 principle 升级为 active（进入 prompt 注入候选池）。\
        用法：reflection 时同一条 principle 被命中≥3次、或用户复述时调。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"principle_id": {"type": "string"}},
            "required": ["principle_id"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let id_raw = match parse_required_string(&input, "principle_id") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let id = PrincipleId::from_string(id_raw);
        if let Err(e) = principle_repo::update_principle_state(&self.app, &id, PrincipleState::Active) {
            return err_text(format!("升级 principle 失败：{e}"));
        }
        (ok_json(json!({"ok": true, "principle_id": id.as_str(), "state": "active"})), false)
    }
}

// ====== retire_principle ================================================

pub struct RetirePrincipleTool {
    app: AppHandle,
}

impl RetirePrincipleTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for RetirePrincipleTool {
    fn name(&self) -> &'static str {
        "retire_principle"
    }

    fn description(&self) -> &'static str {
        "软删除一条 principle（state→retired）。\
        用法：reflection 发现某条原则反复打脸 / 用户明确撤回 / 与新原则冲突且新的更准时调。\
        必填：principle_id, reason。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "principle_id": {"type": "string"},
                "reason": {"type": "string"}
            },
            "required": ["principle_id", "reason"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let id_raw = match parse_required_string(&input, "principle_id") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let _reason = match parse_required_string(&input, "reason") {
            Ok(s) => s,
            Err(e) => return err_text(e),
        };
        let id = PrincipleId::from_string(id_raw);
        if let Err(e) = principle_repo::update_principle_state(&self.app, &id, PrincipleState::Retired) {
            return err_text(format!("退役 principle 失败：{e}"));
        }
        (ok_json(json!({"ok": true, "principle_id": id.as_str(), "state": "retired"})), false)
    }
}
