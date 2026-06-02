//! SkillStore —— 扫描 skills 目录、解析 SKILL.md frontmatter、提供索引 + 正文读取。
//!
//! Spec: docs/design/agent-runtime-module.md §Skills（playbook，渐进披露）
//!
//! 两层模型：Tool = 原语（注册 handler），Skill = playbook（`SKILL.md`，不是 handler）。
//! 渐进披露：system prompt 只放 skill 索引（name + description），正文由 `run_skill` fork 的子 agent 读取。
//!
//! 存盘约定：`<skills_dir>/<name>/SKILL.md`，YAML frontmatter（`name` + `description`）+ markdown 正文。
//! frontmatter 用手写轻量解析（只需两个 string 字段；项目无 serde_yaml 依赖，不为此新增）。

use crate::domain::shared::ErrorCode;
use std::path::{Path, PathBuf};

/// 一条 skill 索引项（喂 system prompt 用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillIndexEntry {
    pub name: String,
    pub description: String,
}

/// SkillStore —— skills 目录的只读访问 + frontmatter 解析。
///
/// `skills_dir` 由 adapter / bootstrap 注入绝对路径（domain 不感知）。
#[derive(Debug, Clone)]
pub struct SkillStore {
    skills_dir: PathBuf,
}

impl SkillStore {
    pub fn new(skills_dir: PathBuf) -> Self {
        Self { skills_dir }
    }

    pub fn skills_dir(&self) -> &Path {
        &self.skills_dir
    }

    /// 某 skill 的 SKILL.md 绝对路径。
    pub fn skill_md_path(&self, name: &str) -> PathBuf {
        self.skills_dir.join(name).join("SKILL.md")
    }

    /// 扫描 skills_dir 下所有 `<name>/SKILL.md`，解析 frontmatter，返回索引。
    ///
    /// 目录不存在 / 为空 → 空索引（skills 初始为空，spec §Skills）。
    /// 按 name 字典序（prompt cache 稳定）。
    /// 解析失败 / 缺字段的条目跳过（不让一个坏 SKILL.md 拖垮整个索引）。
    pub fn list_index(&self) -> Vec<SkillIndexEntry> {
        let mut out: Vec<SkillIndexEntry> = Vec::new();
        let read_dir = match std::fs::read_dir(&self.skills_dir) {
            Ok(rd) => rd,
            Err(_) => return out, // 目录不存在 = 空索引
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let md = path.join("SKILL.md");
            let content = match std::fs::read_to_string(&md) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if let Some((name, description)) = parse_frontmatter(&content) {
                if !name.is_empty() && !description.is_empty() {
                    out.push(SkillIndexEntry { name, description });
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// 读某 skill 的完整 SKILL.md 正文（含 frontmatter，让模型看到完整 playbook）。
    ///
    /// 不存在 → `not_found`。
    pub fn read_body(&self, name: &str) -> Result<String, ErrorCode> {
        let md = self.skill_md_path(name);
        std::fs::read_to_string(&md).map_err(|_| ErrorCode::NotFound)
    }
}

/// 解析 SKILL.md 的 YAML frontmatter，提取 `name` / `description`。
///
/// 支持的最小 YAML 子集：以 `---` 行开头、`---` 行结束的块，块内 `key: value` 行。
/// value 可带可选引号（单/双）。只取 `name` / `description` 两个 string 字段。
/// 不是为通用 YAML——只覆盖 SKILL.md frontmatter 的 string 标量场景。
pub fn parse_frontmatter(content: &str) -> Option<(String, String)> {
    let mut lines = content.lines();
    // 第一行必须是 frontmatter 分隔符（允许前导空白行）。
    let mut first = lines.next()?;
    while first.trim().is_empty() {
        first = lines.next()?;
    }
    if first.trim() != "---" {
        return None;
    }
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    for line in lines {
        if line.trim() == "---" {
            break; // frontmatter 结束
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            let val = unquote_scalar(v.trim());
            match key {
                "name" => name = Some(val),
                "description" => description = Some(val),
                _ => {}
            }
        }
    }
    Some((name.unwrap_or_default(), description.unwrap_or_default()))
}

/// 去掉成对的单 / 双引号（YAML 标量）并对双引号标量做反转义（`\"` → `"`、`\\` → `\`）。
/// 单引号标量按 YAML 字面处理（不反转义）。无引号原样返回。
fn unquote_scalar(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if first == b'"' && last == b'"' {
            // 双引号：反转义 \\ 和 \"
            let inner = &s[1..s.len() - 1];
            let mut out = String::with_capacity(inner.len());
            let mut chars = inner.chars();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    match chars.next() {
                        Some('"') => out.push('"'),
                        Some('\\') => out.push('\\'),
                        Some(other) => {
                            out.push('\\');
                            out.push(other);
                        }
                        None => out.push('\\'),
                    }
                } else {
                    out.push(c);
                }
            }
            return out;
        }
        if first == b'\'' && last == b'\'' {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// 渲染 SKILL.md 文件全文（frontmatter + body）。
///
/// 给 create_skill 写盘用。frontmatter 只含 `name` / `description` 两个字段。
/// description / body 按 YAML 安全：description 用双引号包裹并转义内部双引号（单行标量）。
pub fn render_skill_md(name: &str, description: &str, body: &str) -> String {
    let esc_desc = description.replace('\\', "\\\\").replace('"', "\\\"");
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str("name: ");
    out.push_str(name);
    out.push('\n');
    out.push_str("description: \"");
    out.push_str(&esc_desc);
    out.push_str("\"\n");
    out.push_str("---\n\n");
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_skills_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gangzi-skillstore-test-{}-{}",
            tag,
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(dir: &Path, name: &str, content: &str) {
        let sd = dir.join(name);
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(sd.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn parse_frontmatter_basic() {
        let c = "---\nname: alpha\ndescription: do a thing\n---\n\nbody here";
        let (n, d) = parse_frontmatter(c).unwrap();
        assert_eq!(n, "alpha");
        assert_eq!(d, "do a thing");
    }

    #[test]
    fn parse_frontmatter_quoted() {
        let c = "---\nname: \"beta\"\ndescription: 'has: colon'\n---\nbody";
        let (n, d) = parse_frontmatter(c).unwrap();
        assert_eq!(n, "beta");
        assert_eq!(d, "has: colon");
    }

    #[test]
    fn parse_frontmatter_missing_delimiter_returns_none() {
        assert!(parse_frontmatter("no frontmatter here").is_none());
    }

    #[test]
    fn render_then_parse_roundtrip() {
        let md = render_skill_md("gamma", "a \"quoted\" desc", "# Body\n\nsteps");
        let (n, d) = parse_frontmatter(&md).unwrap();
        assert_eq!(n, "gamma");
        assert_eq!(d, "a \"quoted\" desc");
        assert!(md.contains("# Body"));
    }

    #[test]
    fn list_index_empty_when_dir_missing() {
        let store = SkillStore::new(std::env::temp_dir().join(format!("nope-{}", uuid::Uuid::new_v4())));
        assert!(store.list_index().is_empty());
    }

    #[test]
    fn list_index_sorted_and_skips_invalid() {
        let dir = temp_skills_dir("index");
        write_skill(&dir, "zebra", "---\nname: zebra\ndescription: z\n---\nbody");
        write_skill(&dir, "alpha", "---\nname: alpha\ndescription: a\n---\nbody");
        write_skill(&dir, "broken", "no frontmatter"); // skipped
        let store = SkillStore::new(dir.clone());
        let idx = store.list_index();
        assert_eq!(idx.len(), 2);
        assert_eq!(idx[0].name, "alpha");
        assert_eq!(idx[1].name, "zebra");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_body_returns_full_content() {
        let dir = temp_skills_dir("readbody");
        let full = "---\nname: alpha\ndescription: a\n---\n\n# Playbook\nstep 1";
        write_skill(&dir, "alpha", full);
        let store = SkillStore::new(dir.clone());
        let body = store.read_body("alpha").unwrap();
        assert_eq!(body, full);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_body_missing_is_not_found() {
        let dir = temp_skills_dir("missing");
        let store = SkillStore::new(dir.clone());
        assert_eq!(store.read_body("ghost"), Err(ErrorCode::NotFound));
        std::fs::remove_dir_all(&dir).ok();
    }
}
