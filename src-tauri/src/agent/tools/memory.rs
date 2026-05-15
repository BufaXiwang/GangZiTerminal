//! update_memory / remove_memory——投资者长期记忆的写入工具。
//!
//! 取代旧的"agent 在最终文本里塞 JSON、后端正则切"的脆弱协议。现在 agent 显式
//! 调工具来更新记忆，每次调用都对应一条 ToolStart/ToolEnd 事件，UI 能看到。
//!
//! 字段结构对齐 [`InvestorMemoryUpdate`]——focus_themes / preferred_markets /
//! risk_preference / learning_goals / known_biases / investment_principles /
//! watch_questions / recent_insights。每条 80 字上限、按 list cap 截断的逻辑
//! 全部继承自 [`crate::memory::merge_investor_memory`]。

use crate::agent::tools::{err_text, ok_json, Tool, ToolContext};
use crate::agent::types::ToolResultContent;
use crate::agent_io::InvestorMemoryUpdate;
use crate::memory::merge_investor_memory;
use crate::pipeline::{read_investor_memory, save_investor_memory};
use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::AppHandle;

fn input_schema_object() -> Value {
    let str_list = json!({
        "type": "array",
        "items": { "type": "string", "maxLength": 80 }
    });
    json!({
        "type": "object",
        "properties": {
            "focusThemes":           str_list,
            "preferredMarkets":      str_list,
            "riskPreference":        { "type": "string", "maxLength": 80 },
            "learningGoals":         str_list,
            "knownBiases":           str_list,
            "investmentPrinciples":  str_list,
            "watchQuestions":        str_list,
            "recentInsights":        str_list
        },
        "additionalProperties": false
    })
}

fn parse_update(input: &Value) -> Result<InvestorMemoryUpdate, String> {
    serde_json::from_value(input.clone()).map_err(|err| format!("memory update 解析失败：{err}"))
}

// ===== update_memory =====================================================

pub struct UpdateMemoryTool {
    app: AppHandle,
}

impl UpdateMemoryTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for UpdateMemoryTool {
    fn name(&self) -> &'static str {
        "update_memory"
    }

    fn description(&self) -> &'static str {
        // 静态字符串里嵌可读说明，方便 agent 决定何时调
        "向投资者长期记忆追加新条目。根据用户对话或复盘新得出的认知（关注主题、\
        投资原则、风险偏好变化、近期 insight 等）调用——一次调用一个或多个字段。\
        新条目会去重、80 字截断、按 list 上限保留最新。\
        可写字段：focusThemes / preferredMarkets / riskPreference / learningGoals / \
        knownBiases / investmentPrinciples / watchQuestions / recentInsights。\
        列表字段是字符串数组（每条 ≤80 字），riskPreference 是单字符串。"
    }

    fn input_schema(&self) -> Value {
        input_schema_object()
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let update = match parse_update(&input) {
            Ok(u) => u,
            Err(e) => return err_text(e),
        };
        let current = read_investor_memory(&self.app);
        let merged = merge_investor_memory(&current, &update, None);
        if let Err(e) = save_investor_memory(&self.app, &merged) {
            return err_text(format!("保存记忆失败：{e}"));
        }
        (
            ok_json(json!({
                "ok": true,
                "applied_fields": fields_present(&input),
                "memory_updated_at": merged.updated_at,
            })),
            false,
        )
    }
}

// ===== remove_memory =====================================================

pub struct RemoveMemoryTool {
    app: AppHandle,
}

impl RemoveMemoryTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for RemoveMemoryTool {
    fn name(&self) -> &'static str {
        "remove_memory"
    }

    fn description(&self) -> &'static str {
        "从投资者长期记忆中删除指定条目（精确字符串匹配）。当用户说'忘了 X'、\
        发现既有记忆已过时或自相矛盾时调用。riskPreference 字段传非空字符串视为\
        清空整个 riskPreference。"
    }

    fn input_schema(&self) -> Value {
        input_schema_object()
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let removal = match parse_update(&input) {
            Ok(u) => u,
            Err(e) => return err_text(e),
        };
        let current = read_investor_memory(&self.app);
        let merged =
            merge_investor_memory(&current, &InvestorMemoryUpdate::default(), Some(&removal));
        if let Err(e) = save_investor_memory(&self.app, &merged) {
            return err_text(format!("保存记忆失败：{e}"));
        }
        (
            ok_json(json!({
                "ok": true,
                "removed_fields": fields_present(&input),
                "memory_updated_at": merged.updated_at,
            })),
            false,
        )
    }
}

fn fields_present(input: &Value) -> Vec<String> {
    input
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_lists_all_8_fields() {
        let schema = input_schema_object();
        let props = schema["properties"].as_object().unwrap();
        for f in [
            "focusThemes",
            "preferredMarkets",
            "riskPreference",
            "learningGoals",
            "knownBiases",
            "investmentPrinciples",
            "watchQuestions",
            "recentInsights",
        ] {
            assert!(props.contains_key(f), "missing field {f}");
        }
    }

    #[test]
    fn parse_update_accepts_partial_input() {
        let input = json!({"focusThemes": ["AI 算力"]});
        let upd = parse_update(&input).unwrap();
        assert_eq!(upd.focus_themes, Some(vec!["AI 算力".to_string()]));
        assert!(upd.preferred_markets.is_none());
    }

    #[test]
    fn parse_update_camel_case_alignment() {
        // JSON Schema 用 camelCase 暴露给 agent，agent_io 内部 serde 也用 camelCase
        let input = json!({"riskPreference": "稳健", "watchQuestions": ["大盘量能?"]});
        let upd = parse_update(&input).unwrap();
        assert_eq!(upd.risk_preference, Some("稳健".to_string()));
        assert_eq!(upd.watch_questions.as_deref().unwrap().len(), 1);
    }

    #[test]
    fn fields_present_returns_keys() {
        let input = json!({"focusThemes": ["x"], "knownBiases": ["y"]});
        let mut got = fields_present(&input);
        got.sort();
        assert_eq!(got, vec!["focusThemes", "knownBiases"]);
    }
}
