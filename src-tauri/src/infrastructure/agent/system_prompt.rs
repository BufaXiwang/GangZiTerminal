//! SystemPromptBuilder — 把 SkillSpec 集合编译成 system prompt skill 清单。
//!
//! Spec: docs/design/agent-infra-module.md §2 System Prompt Skill 清单，§5 SystemPromptBuilder API
//!
//! 规则：
//! - 固定 protocol 说明前缀，描述 `<use_skill>` / `<skill_result>` 文本协议。
//! - skill 列表按 name **字典序**（保证 prompt cache hit 一致）。
//! - 每个 skill 段：`## <name>` + description + `Input:` schema 摘要 + 至少 1 个 example。
//! - 系统提示词中的 skill 清单部分不允许由 LLM 修改 / 看不见；Runtime 注入后只读。

use crate::domain::agent::SkillSpec;

/// Protocol 前缀（spec §2 System Prompt Skill 清单）。
pub const PROTOCOL_PREAMBLE: &str = concat!(
    "你可以使用以下 skill。要调用某个 skill，输出 XML 标签\n",
    "`<use_skill name=\"...\">{...}</use_skill>`，内容是符合该 skill input schema 的 JSON。\n",
    "每次调用后会以 `<skill_result name=\"...\" call_id=\"...\">` 形式回复给你。",
);

/// 把 enabled skills 按字典序编译成 markdown 清单，prepend protocol 说明 + `base_prompt`。
///
/// Spec §5: Builder 是纯计算，无 I/O。
pub fn build_system_prompt(skills: &[SkillSpec], base_prompt: &str) -> String {
    let mut sorted: Vec<&SkillSpec> = skills.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));

    let mut out = String::new();
    out.push_str(PROTOCOL_PREAMBLE);
    out.push_str("\n\n");
    for sp in sorted {
        out.push_str(&render_skill_section(sp));
        out.push_str("\n\n");
    }
    if !base_prompt.is_empty() {
        out.push_str(base_prompt);
    }
    // trim trailing whitespace
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

fn render_skill_section(sp: &SkillSpec) -> String {
    let mut s = String::new();
    s.push_str("## ");
    s.push_str(&sp.name);
    s.push('\n');
    s.push_str(sp.description.trim());
    s.push('\n');
    // Input schema (compact JSON)
    let schema_str = serde_json::to_string(&sp.input_schema).unwrap_or_else(|_| "{}".into());
    s.push_str("Input: ");
    s.push_str(&schema_str);
    s.push('\n');
    // At least one example
    for ex in &sp.examples {
        s.push_str("Example: ");
        s.push_str(ex);
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::SideEffect;

    fn sp(name: &str, desc: &str) -> SkillSpec {
        SkillSpec::new(
            name,
            desc,
            serde_json::json!({"type":"object"}),
            vec![format!(r#"<use_skill name="{}">{{}}</use_skill>"#, name)],
            5000,
            SideEffect::None,
        )
    }

    #[test]
    fn empty_skill_list_still_emits_protocol_preamble() {
        let s = build_system_prompt(&[], "Base.");
        assert!(s.contains("use_skill"));
        assert!(s.contains("Base."));
    }

    #[test]
    fn skills_emitted_in_alphabetical_order() {
        let skills = vec![
            sp("operate_account", "manage account"),
            sp("fetch_quote", "read quote"),
            sp("news_search", "search news"),
        ];
        let s = build_system_prompt(&skills, "");
        let pos_fetch = s.find("## fetch_quote").unwrap();
        let pos_news = s.find("## news_search").unwrap();
        let pos_op = s.find("## operate_account").unwrap();
        assert!(pos_fetch < pos_news);
        assert!(pos_news < pos_op);
    }

    #[test]
    fn each_skill_gets_description_and_example() {
        let skills = vec![sp("fetch_quote", "Read latest quote")];
        let s = build_system_prompt(&skills, "");
        assert!(s.contains("## fetch_quote"));
        assert!(s.contains("Read latest quote"));
        assert!(s.contains("Input: "));
        assert!(s.contains("Example: <use_skill name=\"fetch_quote\""));
    }

    #[test]
    fn base_prompt_appended_after_skill_list() {
        let skills = vec![sp("x", "x")];
        let s = build_system_prompt(&skills, "BASE_PROMPT_TEXT");
        let p = s.find("BASE_PROMPT_TEXT").unwrap();
        let q = s.find("## x").unwrap();
        assert!(q < p);
    }

    #[test]
    fn protocol_preamble_present_first() {
        let s = build_system_prompt(&[], "");
        assert!(s.starts_with("你可以使用以下 skill"));
    }
}
