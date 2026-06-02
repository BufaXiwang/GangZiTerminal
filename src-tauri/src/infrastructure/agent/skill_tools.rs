//! Skill creator tool —— create_skill / load_skill。
//!
//! Spec: docs/design/agent-runtime-module.md §Skills（playbook，渐进披露）+ §4.2 本地通用 tool 契约
//!
//! 两层模型：Tool = 原语（注册 handler，经 `<use_tool>` 调用），Skill = playbook（`SKILL.md`，不是 handler）。
//! 这两个 tool 是 skill 的**编排工具**：
//! - `create_skill`：模型把一段 playbook 写成 `<skills_dir>/<name>/SKILL.md`（frontmatter + body）。
//! - `load_skill`：模型按 system prompt 里的索引（name + description）按需取某 skill 的完整正文进上下文。
//!
//! 渐进披露：system prompt 只放索引（见 system_prompt.rs），正文不进 prompt，由 load_skill 拉取。
//!
//! 路径安全：`name` 必须是 slug（`^[a-z0-9][a-z0-9-]*$`），防路径穿越（`../`、`/`、绝对路径）。
//! 校验后再次确认目标落在 skills_dir 内（纵深防御）。

use crate::domain::shared::ErrorCode;
use crate::infrastructure::agent::skill_store::{render_skill_md, SkillStore};
use crate::infrastructure::agent::tool_registry::{
    FnToolHandler, RegisterError, ToolHandlerFuture, ToolHandlerOutput, ToolInvocation,
    ToolRegistry,
};
use crate::domain::agent::{SideEffect, ToolSpec};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

const CREATE_TIMEOUT_MS: u64 = 15_000;
const LOAD_TIMEOUT_MS: u64 = 15_000;

// ───────────────────────── slug 校验 ─────────────────────────

/// skill name 必须是 slug：`^[a-z0-9][a-z0-9-]*$`。
/// 防路径穿越：拒绝 `/`、`..`、大写、空、前导 `-`、非法字符。
fn is_valid_skill_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

// ───────────────────────── input DTO ─────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateSkillInput {
    name: String,
    description: String,
    body: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoadSkillInput {
    name: String,
}

fn parse_input<T: for<'de> Deserialize<'de>>(inv: &ToolInvocation) -> Result<T, ToolHandlerOutput> {
    serde_json::from_value::<T>(inv.input.clone()).map_err(|e| {
        ToolHandlerOutput::err(
            serde_json::json!({ "reason": "invalid_input", "message": e.to_string() }),
            ErrorCode::InvalidInput,
        )
    })
}

fn err_out(code: ErrorCode, msg: impl Into<String>) -> ToolHandlerOutput {
    let reason = serde_json::to_value(code)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default();
    ToolHandlerOutput::err(
        serde_json::json!({ "reason": reason, "message": msg.into() }),
        code,
    )
}

// ───────────────────────── handlers ─────────────────────────

async fn handle_create_skill(store: SkillStore, inv: ToolInvocation) -> ToolHandlerOutput {
    let input: CreateSkillInput = match parse_input(&inv) {
        Ok(v) => v,
        Err(e) => return e,
    };

    if !is_valid_skill_name(&input.name) {
        return err_out(
            ErrorCode::InvalidInput,
            "skill name must match ^[a-z0-9][a-z0-9-]*$ (slug; no slashes / uppercase / dots)",
        );
    }
    if input.description.trim().is_empty() {
        return err_out(ErrorCode::InvalidInput, "description must not be empty");
    }

    let md_path = store.skill_md_path(&input.name);

    // 纵深防御：确认目标确实落在 skills_dir 内（slug 已防穿越，这是二次确认）。
    if !md_path.starts_with(store.skills_dir()) {
        return err_out(
            ErrorCode::InvalidInput,
            "resolved skill path escapes skills directory",
        );
    }

    let existed = md_path.exists();

    if let Some(parent) = md_path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return err_out(
                ErrorCode::InvalidInput,
                format!("cannot create skill dir: {e}"),
            );
        }
    }

    let content = render_skill_md(&input.name, &input.description, &input.body);
    if let Err(e) = tokio::fs::write(&md_path, content.as_bytes()).await {
        return err_out(ErrorCode::InvalidInput, format!("write failed: {e}"));
    }

    ToolHandlerOutput::ok(serde_json::json!({
        "path": md_path.to_string_lossy(),
        "created": !existed,
    }))
}

async fn handle_load_skill(store: SkillStore, inv: ToolInvocation) -> ToolHandlerOutput {
    let input: LoadSkillInput = match parse_input(&inv) {
        Ok(v) => v,
        Err(e) => return e,
    };

    if !is_valid_skill_name(&input.name) {
        return err_out(
            ErrorCode::InvalidInput,
            "skill name must match ^[a-z0-9][a-z0-9-]*$",
        );
    }

    match store.read_body(&input.name) {
        Ok(content) => ToolHandlerOutput::ok(serde_json::json!({
            "name": input.name,
            "content": content,
        })),
        Err(code) => err_out(code, format!("skill not found: {}", input.name)),
    }
}

// ───────────────────────── 注册 ─────────────────────────

fn handler_for<F, Fut>(
    store: SkillStore,
    f: F,
) -> Arc<FnToolHandler<impl Fn(ToolInvocation) -> ToolHandlerFuture + Send + Sync + 'static>>
where
    F: Fn(SkillStore, ToolInvocation) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = ToolHandlerOutput> + Send + 'static,
{
    Arc::new(FnToolHandler(move |inv: ToolInvocation| {
        let store = store.clone();
        let f = f.clone();
        Box::pin(async move { f(store, inv).await }) as ToolHandlerFuture
    }))
}

/// 把 create_skill / load_skill 注册进 registry。
///
/// Spec: agent-runtime-module.md §Skills + §4.2。`skills_dir` 由 adapter / bootstrap 注入绝对路径。
/// 注册时确保 skills 目录存在（create_skill 写盘需要其父链）。
pub fn register_skill_tools(
    registry: &ToolRegistry,
    skills_dir: PathBuf,
) -> Result<(), RegisterError> {
    let _ = std::fs::create_dir_all(&skills_dir);
    let store = SkillStore::new(skills_dir);

    registry.register_tool(
        tool_spec_create_skill(),
        handler_for(store.clone(), |s, inv| handle_create_skill(s, inv)),
    )?;
    registry.register_tool(
        tool_spec_load_skill(),
        handler_for(store, |s, inv| handle_load_skill(s, inv)),
    )?;
    Ok(())
}

fn tool_spec_create_skill() -> ToolSpec {
    ToolSpec::new(
        "create_skill",
        "把一段可复用的 playbook 沉淀成 skill：写 <skills_dir>/<name>/SKILL.md（frontmatter name+description + markdown body）。name 必须是 slug（^[a-z0-9][a-z0-9-]*$）；description 非空；同名覆盖更新（created=false）。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "slug，^[a-z0-9][a-z0-9-]*$" },
                "description": { "type": "string", "description": "一句话说明该 skill 干什么（进 system prompt 索引，非空）" },
                "body": { "type": "string", "description": "markdown 正文：完成任务时如何编排 tool" }
            },
            "required": ["name", "description", "body"]
        }),
        vec![r##"<use_tool name="create_skill">{"name":"momentum-scan","description":"扫动量候选并形成判断","body":"# 动量扫描\n1. fetch_quotes scan ..."}</use_tool>"##.into()],
        CREATE_TIMEOUT_MS,
        SideEffect::NonTradingWrite,
    )
}

fn tool_spec_load_skill() -> ToolSpec {
    ToolSpec::new(
        "load_skill",
        "按 name 取某 skill 的完整 SKILL.md 正文（含 frontmatter）进上下文。system prompt 只给 skill 索引（name+description）；要按某 skill 行事先 load 它。不存在 → not_found。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "skill slug（见 system prompt 的可用 Skill 索引）" }
            },
            "required": ["name"]
        }),
        vec![r#"<use_tool name="load_skill">{"name":"momentum-scan"}</use_tool>"#.into()],
        LOAD_TIMEOUT_MS,
        SideEffect::None,
    )
}

// ───────────────────────── tests (hermetic) ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::agent::skill_store::SkillStore;

    fn temp_skills_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gangzi-skilltools-test-{}-{}",
            tag,
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn inv(input: serde_json::Value) -> ToolInvocation {
        ToolInvocation {
            run_id: "r1".into(),
            tool_call_id: "tc_1".into(),
            name: "x".into(),
            input,
        }
    }

    #[test]
    fn slug_validation() {
        assert!(is_valid_skill_name("alpha"));
        assert!(is_valid_skill_name("momentum-scan-2"));
        assert!(is_valid_skill_name("9lives"));
        assert!(!is_valid_skill_name(""));
        assert!(!is_valid_skill_name("Alpha")); // uppercase
        assert!(!is_valid_skill_name("../escape"));
        assert!(!is_valid_skill_name("a/b"));
        assert!(!is_valid_skill_name("-leading"));
        assert!(!is_valid_skill_name("with space"));
        assert!(!is_valid_skill_name("dot.name"));
    }

    #[tokio::test]
    async fn create_then_index_then_load_roundtrip() {
        let dir = temp_skills_dir("roundtrip");
        let out = handle_create_skill(
            SkillStore::new(dir.clone()),
            inv(serde_json::json!({
                "name": "alpha",
                "description": "do alpha things",
                "body": "# Alpha\nstep one"
            })),
        )
        .await;
        assert!(!out.is_error, "{:?}", out.output_summary);
        assert_eq!(out.output_summary["created"], true);

        // index lists (name, description)
        let store = SkillStore::new(dir.clone());
        let idx = store.list_index();
        assert_eq!(idx.len(), 1);
        assert_eq!(idx[0].name, "alpha");
        assert_eq!(idx[0].description, "do alpha things");

        // load returns full body
        let out = handle_load_skill(
            SkillStore::new(dir.clone()),
            inv(serde_json::json!({ "name": "alpha" })),
        )
        .await;
        assert!(!out.is_error);
        assert_eq!(out.output_summary["name"], "alpha");
        let content = out.output_summary["content"].as_str().unwrap();
        assert!(content.contains("# Alpha"));
        assert!(content.contains("description:"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn create_invalid_name_rejected_no_file() {
        let dir = temp_skills_dir("invalid-name");
        for bad in ["../escape", "Bad", "", "a/b"] {
            let out = handle_create_skill(
                SkillStore::new(dir.clone()),
                inv(serde_json::json!({ "name": bad, "description": "d", "body": "b" })),
            )
            .await;
            assert!(out.is_error, "name {bad:?} should be rejected");
            assert_eq!(out.error_code, Some(ErrorCode::InvalidInput));
        }
        // nothing escaped the skills dir
        let parent = dir.parent().unwrap();
        assert!(!parent.join("escape").join("SKILL.md").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn create_empty_description_rejected() {
        let dir = temp_skills_dir("empty-desc");
        let out = handle_create_skill(
            SkillStore::new(dir.clone()),
            inv(serde_json::json!({ "name": "alpha", "description": "   ", "body": "b" })),
        )
        .await;
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::InvalidInput));
        assert!(!dir.join("alpha").join("SKILL.md").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn create_twice_reports_created_false() {
        let dir = temp_skills_dir("overwrite");
        let mk = |body: &str| {
            inv(serde_json::json!({ "name": "alpha", "description": "d", "body": body }))
        };
        let out1 = handle_create_skill(SkillStore::new(dir.clone()), mk("v1")).await;
        assert_eq!(out1.output_summary["created"], true);
        let out2 = handle_create_skill(SkillStore::new(dir.clone()), mk("v2")).await;
        assert!(!out2.is_error);
        assert_eq!(out2.output_summary["created"], false);
        // content updated
        let content = std::fs::read_to_string(dir.join("alpha").join("SKILL.md")).unwrap();
        assert!(content.contains("v2"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn load_missing_is_not_found() {
        let dir = temp_skills_dir("load-missing");
        let out = handle_load_skill(
            SkillStore::new(dir.clone()),
            inv(serde_json::json!({ "name": "ghost" })),
        )
        .await;
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::NotFound));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn register_skill_tools_registers_two() {
        let dir = temp_skills_dir("register");
        let registry = ToolRegistry::new_without_persist();
        register_skill_tools(&registry, dir.clone()).unwrap();
        assert!(registry.has_tool("create_skill"));
        assert!(registry.has_tool("load_skill"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
