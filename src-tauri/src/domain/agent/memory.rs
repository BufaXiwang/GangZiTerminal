//! 投资者长期记忆——agent 通过 `update_memory` / `remove_memory` 工具维护的可学习状态。
//!
//! - `InvestorMemory`：完整快照（落库形态）
//! - `InvestorMemoryUpdate`：增量更新形态（工具入参）
//! - `merge_investor_memory`：纯函数 merge——新增前置 + 去重 + 每条 80 字 cap + 字段 list 上限
//! - `default_investor_memory`：冷启动默认值
//!
//! 删除走 `removals` 同字段名 + 精确字符串匹配。

use serde::{Deserialize, Serialize};

// ====== 类型 ============================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InvestorMemory {
    pub focus_themes: Vec<String>,
    pub preferred_markets: Vec<String>,
    pub risk_preference: String,
    pub learning_goals: Vec<String>,
    pub known_biases: Vec<String>,
    pub investment_principles: Vec<String>,
    pub watch_questions: Vec<String>,
    pub recent_insights: Vec<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InvestorMemoryUpdate {
    pub focus_themes: Option<Vec<String>>,
    pub preferred_markets: Option<Vec<String>>,
    pub risk_preference: Option<String>,
    pub learning_goals: Option<Vec<String>>,
    pub known_biases: Option<Vec<String>>,
    pub investment_principles: Option<Vec<String>>,
    pub watch_questions: Option<Vec<String>>,
    pub recent_insights: Option<Vec<String>>,
}

// ====== merge / default ================================================

const ENTRY_CHAR_CAP: usize = 80;

fn trim_entry(s: &str) -> String {
    let trimmed = s.trim();
    let mut chars = trimmed.chars();
    let mut out = String::new();
    let mut count = 0;
    while count < ENTRY_CHAR_CAP {
        match chars.next() {
            Some(c) => {
                out.push(c);
                count += 1;
            }
            None => break,
        }
    }
    out
}

fn apply(
    existing: &[String],
    incoming: &Option<Vec<String>>,
    dropped: &Option<Vec<String>>,
    limit: usize,
) -> Vec<String> {
    let drop_set: std::collections::HashSet<String> = dropped
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|s| trim_entry(s))
        .filter(|s| !s.is_empty())
        .collect();

    // 顺序：新增在前 + 已有在后；trim + 去空 + 去重 + 截断到 limit
    let mut seen = std::collections::HashSet::new();
    let mut merged: Vec<String> = incoming
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .chain(existing.iter())
        .map(|s| trim_entry(s))
        .filter(|s| !s.is_empty())
        .filter(|s| seen.insert(s.clone()))
        .take(limit)
        .collect();

    merged.retain(|entry| !drop_set.contains(entry));
    merged
}

pub fn default_investor_memory() -> InvestorMemory {
    InvestorMemory {
        focus_themes: Vec::new(),
        preferred_markets: vec!["A股".to_string()],
        risk_preference: "未明确。默认偏学习和验证，不追求自动交易。".to_string(),
        learning_goals: vec!["把市场问题拆成可验证假设。".to_string()],
        known_biases: Vec::new(),
        investment_principles: Vec::new(),
        watch_questions: Vec::new(),
        recent_insights: Vec::new(),
        updated_at: String::new(),
    }
}

pub fn merge_investor_memory(
    current: &InvestorMemory,
    update: &InvestorMemoryUpdate,
    remove: Option<&InvestorMemoryUpdate>,
) -> InvestorMemory {
    let r_focus = remove.and_then(|r| r.focus_themes.clone());
    let r_markets = remove.and_then(|r| r.preferred_markets.clone());
    let r_goals = remove.and_then(|r| r.learning_goals.clone());
    let r_biases = remove.and_then(|r| r.known_biases.clone());
    let r_princ = remove.and_then(|r| r.investment_principles.clone());
    let r_quest = remove.and_then(|r| r.watch_questions.clone());
    let r_insights = remove.and_then(|r| r.recent_insights.clone());

    // riskPreference 是字符串：remove 里出现非空字符串 → 视为清空
    let next_risk = if remove
        .and_then(|r| r.risk_preference.as_deref())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        String::new()
    } else {
        let new_value = update
            .risk_preference
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| trim_entry(s));
        new_value.unwrap_or_else(|| current.risk_preference.clone())
    };

    InvestorMemory {
        focus_themes: apply(&current.focus_themes, &update.focus_themes, &r_focus, 16),
        preferred_markets: apply(
            &current.preferred_markets,
            &update.preferred_markets,
            &r_markets,
            8,
        ),
        risk_preference: next_risk,
        learning_goals: apply(
            &current.learning_goals,
            &update.learning_goals,
            &r_goals,
            12,
        ),
        known_biases: apply(&current.known_biases, &update.known_biases, &r_biases, 12),
        investment_principles: apply(
            &current.investment_principles,
            &update.investment_principles,
            &r_princ,
            18,
        ),
        watch_questions: apply(
            &current.watch_questions,
            &update.watch_questions,
            &r_quest,
            18,
        ),
        recent_insights: apply(
            &current.recent_insights,
            &update.recent_insights,
            &r_insights,
            12,
        ),
        updated_at: chrono::Utc::now().to_rfc3339(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_adds_new_then_trims() {
        let cur = default_investor_memory();
        let update = InvestorMemoryUpdate {
            focus_themes: Some(vec!["AI 算力".into(), "光模块".into()]),
            ..Default::default()
        };
        let merged = merge_investor_memory(&cur, &update, None);
        assert_eq!(merged.focus_themes, vec!["AI 算力", "光模块"]);
    }

    #[test]
    fn merge_removes_explicit_entry() {
        let mut cur = default_investor_memory();
        cur.focus_themes = vec!["A".into(), "B".into(), "C".into()];
        let remove = InvestorMemoryUpdate {
            focus_themes: Some(vec!["B".into()]),
            ..Default::default()
        };
        let merged = merge_investor_memory(&cur, &InvestorMemoryUpdate::default(), Some(&remove));
        assert_eq!(merged.focus_themes, vec!["A", "C"]);
    }

    #[test]
    fn merge_caps_per_entry_to_80_chars() {
        let cur = default_investor_memory();
        let long = "a".repeat(200);
        let update = InvestorMemoryUpdate {
            focus_themes: Some(vec![long.clone()]),
            ..Default::default()
        };
        let merged = merge_investor_memory(&cur, &update, None);
        assert_eq!(merged.focus_themes[0].chars().count(), 80);
    }
}
