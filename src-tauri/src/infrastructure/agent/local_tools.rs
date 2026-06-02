//! 本地通用 tool —— read_file / write_file / edit_file / run_bash。
//!
//! Spec: docs/design/agent-runtime-module.md §「本地通用 tool 与工作区沙箱」 + §4.2 本地通用 tool 契约
//!
//! 约定级沙箱（**非 OS 强隔离**；威胁模型是「引导 agent + 防手滑」，不是「防恶意 / 防被攻破」）：
//! - `read_file` / `run_bash`：路径不受限（read 只读、bash cwd 默认工作区），仍做路径规范化。
//! - `write_file` / `edit_file`：path 规范化后必须落在 `<workspace>` 内，否则 `path_outside_workspace`。
//! - `run_bash`：denylist 拒绝明显危险命令（命中 → `command_rejected` 不执行）；超时 kill；stdout/stderr 截断。
//!
//! 诚实边界（spec §2）：denylist 不可能穷尽，命令可混淆绕过；`run_bash` 不限路径 = 可绕过 write_file 的
//! 工作区限制（bash 本身能写任意路径）。要强隔离需另上 OS 沙箱（macOS seatbelt / Linux landlock）。
//! 这些限制是「结构化写工具的约定」+ 危险命令拦截，不是不可逾越的安全边界。

use crate::domain::agent::SideEffect;
use crate::domain::shared::ErrorCode;
use crate::infrastructure::agent::tool_registry::{
    FnToolHandler, RegisterError, ToolHandlerFuture, ToolHandlerOutput, ToolInvocation,
    ToolRegistry,
};
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// 单次 read_file / run_bash 捕获的字节上限（超出截断，truncated=true）。
const READ_MAX_BYTES: usize = 256 * 1024; // 256KB
const BASH_OUTPUT_MAX_BYTES: usize = 64 * 1024; // 64KB / 流

/// 各 tool 默认 timeout（ToolSpec.timeout_ms）。
const READ_TIMEOUT_MS: u64 = 15_000;
const WRITE_TIMEOUT_MS: u64 = 15_000;
const EDIT_TIMEOUT_MS: u64 = 15_000;
const BASH_TIMEOUT_MS: u64 = 30_000;

// ───────────────────────── 路径规范化 / 工作区校验 ─────────────────────────

/// 纯字符串层规范化：相对 path 以 `base` 为基拼接；展开 `.` / `..`；折叠重复分隔符。
///
/// Spec §2「工作区路径强制规则」step 1：规范化纯字符串层先做一遍（不触 FS）。
fn normalize_path(raw: &str, base: &Path) -> PathBuf {
    let p = Path::new(raw);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    let mut out = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // 弹出上一段；若已到根（无可弹）则忽略（不能越过根）。
                if !out.pop() {
                    // 保留根 prefix（如 "/"）——pop 在仅有 RootDir 时返回 false。
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// write/edit 工作区前缀校验。
///
/// Spec §2 step 2/3：规范化后绝对路径必须以 `<workspace>` + 分隔符为前缀（或正好等于工作区下某文件）；
/// `..` 逃逸一律拒绝。symlink：对已存在的父链做一次 canonicalize 校验，解析后真实路径仍须在工作区内。
/// 约定级：不保证防 TOCTOU。
fn ensure_in_workspace(path: &Path, workspace: &Path) -> Result<(), ErrorCode> {
    // 1. 字符串层前缀校验（已规范化的 path）。
    if !path.starts_with(workspace) {
        return Err(ErrorCode::PathOutsideWorkspace);
    }
    // 2. symlink 防逃逸：canonicalize 已存在的最近父链，确认真实路径仍在工作区内。
    //    新文件本身可能不存在 → 退而 canonicalize 其父目录。
    let probe = if path.exists() { path } else { path.parent().unwrap_or(path) };
    if let Ok(real) = std::fs::canonicalize(probe) {
        // 工作区本身也 canonicalize，避免 /var ↔ /private/var (macOS) 这类等价前缀不匹配。
        let real_ws = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        if !real.starts_with(&real_ws) {
            return Err(ErrorCode::PathOutsideWorkspace);
        }
    }
    Ok(())
}

// ───────────────────────── run_bash 危险命令门禁（约定级 denylist） ─────────────────────────

/// 命中 = 拒绝执行。Spec §2「run_bash 危险命令门禁」必拦清单（非穷举，约定级）。
fn is_dangerous_command(cmd: &str) -> bool {
    let c = cmd.to_lowercase();
    // 递归删除
    if c.contains("rm -rf") || c.contains("rm -fr") || c.contains("rm -r ") || c.contains("rm --recursive") {
        return true;
    }
    // 管道执行远端脚本：curl/wget ... | sh|bash
    let piped_remote = (c.contains("curl") || c.contains("wget"))
        && c.contains('|')
        && (c.contains("sh") || c.contains("bash"));
    if piped_remote {
        return true;
    }
    // 提权
    if c.contains("sudo ") || c.starts_with("sudo") || c.contains(" su ") || c.starts_with("su ") {
        return true;
    }
    // 磁盘 / 系统破坏
    if c.contains("mkfs") || c.contains("dd ") || c.contains(" of=/") {
        return true;
    }
    // fork bomb
    if c.contains(":(){") || c.contains(":(){:") || c.contains(":(){ :") {
        return true;
    }
    // 写 / 重定向到工作区外的系统目录
    if c.contains("> /etc")
        || c.contains(">> /etc")
        || c.contains("> /usr")
        || c.contains("> /bin")
        || c.contains("> /sys")
        || c.contains("> /dev")
        || c.contains("tee /etc")
        || c.contains("tee /usr")
    {
        return true;
    }
    // 包管理 / 系统改动
    if c.contains("npm i -g")
        || c.contains("npm install -g")
        || c.contains("apt ")
        || c.contains("apt-get")
        || c.contains("brew ")
        || c.contains("launchctl")
        || c.contains("chmod -r 777")
    {
        return true;
    }
    false
}

// ───────────────────────── input DTO ─────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadFileInput {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteFileInput {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditFileInput {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunBashInput {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
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

async fn handle_read_file(workspace: PathBuf, inv: ToolInvocation) -> ToolHandlerOutput {
    let input: ReadFileInput = match parse_input(&inv) {
        Ok(v) => v,
        Err(e) => return e,
    };
    // read 路径不受工作区限制，但仍规范化（避免畸形 path 直接交 OS）。
    let path = normalize_path(&input.path, &workspace);
    let meta = match tokio::fs::metadata(&path).await {
        Ok(m) => m,
        Err(_) => return err_out(ErrorCode::NotFound, format!("path not found: {}", path.display())),
    };
    if meta.is_dir() {
        return err_out(ErrorCode::InvalidInput, "path is a directory");
    }

    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => return err_out(ErrorCode::InvalidInput, format!("read failed: {e}")),
    };

    // 二进制 / 非 UTF-8 优雅处理：尝试 lossless UTF-8；失败 → invalid_input（非文本）。
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => return err_out(ErrorCode::InvalidInput, "file is not valid UTF-8 text"),
    };

    // 按行 offset/limit 选取（offset/limit 缺省 = 全文）。
    let mut truncated = false;
    let content = if input.offset.is_some() || input.limit.is_some() {
        let start = input.offset.unwrap_or(0);
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        let end = match input.limit {
            Some(l) => (start + l).min(total),
            None => total,
        };
        if end < total {
            truncated = true;
        }
        let slice_start = start.min(total);
        lines[slice_start..end.max(slice_start)].join("\n")
    } else {
        text
    };

    // 字节上限截断（防 OOM / 巨型 payload）。
    let (content, byte_truncated) = if content.len() > READ_MAX_BYTES {
        let mut cut = READ_MAX_BYTES;
        while !content.is_char_boundary(cut) && cut > 0 {
            cut -= 1;
        }
        (content[..cut].to_string(), true)
    } else {
        (content, false)
    };
    truncated = truncated || byte_truncated;

    ToolHandlerOutput::ok(serde_json::json!({
        "content": content,
        "truncated": truncated,
    }))
}

async fn handle_write_file(workspace: PathBuf, inv: ToolInvocation) -> ToolHandlerOutput {
    let input: WriteFileInput = match parse_input(&inv) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let path = normalize_path(&input.path, &workspace);
    if let Err(code) = ensure_in_workspace(&path, &workspace) {
        return err_out(code, format!("path outside workspace: {}", path.display()));
    }
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return err_out(ErrorCode::InvalidInput, format!("cannot create parent dir: {e}"));
        }
    }
    let bytes = input.content.as_bytes();
    if let Err(e) = tokio::fs::write(&path, bytes).await {
        return err_out(ErrorCode::InvalidInput, format!("write failed: {e}"));
    }
    ToolHandlerOutput::ok(serde_json::json!({ "bytesWritten": bytes.len() }))
}

async fn handle_edit_file(workspace: PathBuf, inv: ToolInvocation) -> ToolHandlerOutput {
    let input: EditFileInput = match parse_input(&inv) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let path = normalize_path(&input.path, &workspace);
    if let Err(code) = ensure_in_workspace(&path, &workspace) {
        return err_out(code, format!("path outside workspace: {}", path.display()));
    }
    if !path.exists() {
        return err_out(ErrorCode::NotFound, format!("file not found: {}", path.display()));
    }
    let original = match tokio::fs::read_to_string(&path).await {
        Ok(s) => s,
        Err(e) => return err_out(ErrorCode::InvalidInput, format!("read failed: {e}")),
    };

    let occurrences = original.matches(&input.old_string).count();
    if occurrences == 0 {
        return err_out(ErrorCode::InvalidInput, "oldString not found in file");
    }
    if !input.replace_all && occurrences > 1 {
        return err_out(
            ErrorCode::InvalidInput,
            format!("oldString is not unique (matched {occurrences} times); set replaceAll to replace all"),
        );
    }

    let (new_content, replaced) = if input.replace_all {
        (original.replace(&input.old_string, &input.new_string), occurrences)
    } else {
        (original.replacen(&input.old_string, &input.new_string, 1), 1)
    };

    if let Err(e) = tokio::fs::write(&path, new_content.as_bytes()).await {
        return err_out(ErrorCode::InvalidInput, format!("write failed: {e}"));
    }
    ToolHandlerOutput::ok(serde_json::json!({ "replaced": replaced }))
}

async fn handle_run_bash(workspace: PathBuf, inv: ToolInvocation) -> ToolHandlerOutput {
    let input: RunBashInput = match parse_input(&inv) {
        Ok(v) => v,
        Err(e) => return e,
    };

    // 危险命令门禁（约定级 denylist）：命中 → 不执行，返回 command_rejected。
    if is_dangerous_command(&input.command) {
        return err_out(
            ErrorCode::CommandRejected,
            "command rejected by danger denylist (convention-level guard, not OS isolation)",
        );
    }

    // cwd 默认工作区；可指向工作区外（路径不受限，见诚实边界）。仍规范化。
    let cwd = match &input.cwd {
        Some(c) => normalize_path(c, &workspace),
        None => workspace.clone(),
    };
    if !cwd.is_dir() {
        return err_out(ErrorCode::InvalidInput, format!("cwd not a directory: {}", cwd.display()));
    }

    let timeout_ms = input.timeout_ms.unwrap_or(BASH_TIMEOUT_MS);

    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(&input.command)
        .current_dir(&cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return err_out(ErrorCode::InvalidInput, format!("spawn failed: {e}")),
    };

    let output = match tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        child.wait_with_output(),
    )
    .await
    {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return err_out(ErrorCode::InvalidInput, format!("exec failed: {e}")),
        Err(_) => {
            // 超时：kill_on_drop 会终止子进程。
            return ToolHandlerOutput::err(
                serde_json::json!({ "reason": "tool_timeout", "timeoutMs": timeout_ms }),
                ErrorCode::ToolTimeout,
            );
        }
    };

    let (stdout, t1) = truncate_bytes(&output.stdout);
    let (stderr, t2) = truncate_bytes(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);

    ToolHandlerOutput::ok(serde_json::json!({
        "stdout": stdout,
        "stderr": stderr,
        "exitCode": exit_code,
        "truncated": t1 || t2,
    }))
}

/// 截断到字节上限，返回 (lossy UTF-8 文本, 是否截断)。
fn truncate_bytes(raw: &[u8]) -> (String, bool) {
    if raw.len() <= BASH_OUTPUT_MAX_BYTES {
        (String::from_utf8_lossy(raw).into_owned(), false)
    } else {
        (
            String::from_utf8_lossy(&raw[..BASH_OUTPUT_MAX_BYTES]).into_owned(),
            true,
        )
    }
}

// ───────────────────────── 注册 ─────────────────────────

fn handler_for<F, Fut>(workspace: PathBuf, f: F) -> Arc<FnToolHandler<impl Fn(ToolInvocation) -> ToolHandlerFuture + Send + Sync + 'static>>
where
    F: Fn(PathBuf, ToolInvocation) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = ToolHandlerOutput> + Send + 'static,
{
    Arc::new(FnToolHandler(move |inv: ToolInvocation| {
        let ws = workspace.clone();
        let f = f.clone();
        Box::pin(async move { f(ws, inv).await }) as ToolHandlerFuture
    }))
}

/// 把 4 个本地通用 tool 注册进 registry。
///
/// Spec: agent-runtime-module.md §4.2。`workspace_dir` 由 adapter 注入绝对路径（domain 不感知）。
/// 注册时确保工作区目录存在（write_file 的父链需要它）。
pub fn register_local_tools(
    registry: &ToolRegistry,
    workspace_dir: PathBuf,
) -> Result<(), RegisterError> {
    // 确保工作区目录存在；忽略已存在错误。失败不阻断注册（写时会再次报错）。
    let _ = std::fs::create_dir_all(&workspace_dir);

    // read_file
    registry.register_tool(
        tool_spec_read(),
        handler_for(workspace_dir.clone(), |ws, inv| handle_read_file(ws, inv)),
    )?;
    // write_file
    registry.register_tool(
        tool_spec_write(),
        handler_for(workspace_dir.clone(), |ws, inv| handle_write_file(ws, inv)),
    )?;
    // edit_file
    registry.register_tool(
        tool_spec_edit(),
        handler_for(workspace_dir.clone(), |ws, inv| handle_edit_file(ws, inv)),
    )?;
    // run_bash
    registry.register_tool(
        tool_spec_bash(),
        handler_for(workspace_dir, |ws, inv| handle_run_bash(ws, inv)),
    )?;
    Ok(())
}

use crate::domain::agent::ToolSpec;

#[allow(non_snake_case)]
fn tool_spec_read() -> ToolSpec {
    ToolSpec::new(
        "read_file",
        "读任意 path 的文本文件（只读，不受工作区限制）。可选 offset/limit 按行读；超大文件按字节上限截断（truncated=true）。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "绝对或相对（相对以 workspace 为基）" },
                "offset": { "type": "integer", "description": "起始行（可选）" },
                "limit": { "type": "integer", "description": "读取行数上限（可选）" }
            },
            "required": ["path"]
        }),
        vec![r#"<use_tool name="read_file">{"path":"/etc/hosts","limit":50}</use_tool>"#.into()],
        READ_TIMEOUT_MS,
        SideEffect::None,
    )
}

#[allow(non_snake_case)]
fn tool_spec_write() -> ToolSpec {
    ToolSpec::new(
        "write_file",
        "写文件（path 规范化后必须落在 workspace 内，否则 path_outside_workspace）。覆盖写。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "规范化后必须在 workspace 内" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        }),
        vec![r##"<use_tool name="write_file">{"path":"notes/research.md","content":"# notes"}</use_tool>"##.into()],
        WRITE_TIMEOUT_MS,
        SideEffect::NonTradingWrite,
    )
}

#[allow(non_snake_case)]
fn tool_spec_edit() -> ToolSpec {
    ToolSpec::new(
        "edit_file",
        "定向替换文件内容（path 必须在 workspace 内）。replaceAll=false（默认）时 oldString 必须唯一命中。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "规范化后必须在 workspace 内" },
                "oldString": { "type": "string" },
                "newString": { "type": "string" },
                "replaceAll": { "type": "boolean", "description": "默认 false：oldString 必须唯一" }
            },
            "required": ["path", "oldString", "newString"]
        }),
        vec![r#"<use_tool name="edit_file">{"path":"notes.md","oldString":"foo","newString":"bar"}</use_tool>"#.into()],
        EDIT_TIMEOUT_MS,
        SideEffect::NonTradingWrite,
    )
}

#[allow(non_snake_case)]
fn tool_spec_bash() -> ToolSpec {
    ToolSpec::new(
        "run_bash",
        "执行 shell 命令（cwd 默认 workspace，路径不受限）。危险命令门禁拒绝（command_rejected）；超时 kill；输出超长截断。约定级沙箱，非强隔离。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "cwd": { "type": "string", "description": "缺省 = workspace" },
                "timeoutMs": { "type": "integer", "description": "缺省取 ToolSpec.timeoutMs" }
            },
            "required": ["command"]
        }),
        vec![r#"<use_tool name="run_bash">{"command":"ls -la"}</use_tool>"#.into()],
        BASH_TIMEOUT_MS,
        SideEffect::NonTradingWrite,
    )
}

// ───────────────────────── tests (hermetic) ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 造一个唯一临时工作区目录。
    fn temp_workspace(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gangzi-localtools-test-{}-{}",
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

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let ws = temp_workspace("roundtrip");
        let out = handle_write_file(
            ws.clone(),
            inv(serde_json::json!({ "path": "a/b.txt", "content": "hello world" })),
        )
        .await;
        assert!(!out.is_error, "write should succeed: {:?}", out.output_summary);
        assert_eq!(out.output_summary["bytesWritten"], 11);

        let target = ws.join("a/b.txt");
        let out = handle_read_file(ws.clone(), inv(serde_json::json!({ "path": target.to_str().unwrap() }))).await;
        assert!(!out.is_error);
        assert_eq!(out.output_summary["content"], "hello world");

        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn write_outside_workspace_rejected() {
        let ws = temp_workspace("write-outside");
        // absolute outside
        let out = handle_write_file(
            ws.clone(),
            inv(serde_json::json!({ "path": "/tmp/evil-abs.txt", "content": "x" })),
        )
        .await;
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::PathOutsideWorkspace));

        // .. escape
        let out = handle_write_file(
            ws.clone(),
            inv(serde_json::json!({ "path": "../escape.txt", "content": "x" })),
        )
        .await;
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::PathOutsideWorkspace));

        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn edit_outside_workspace_rejected() {
        let ws = temp_workspace("edit-outside");
        let out = handle_edit_file(
            ws.clone(),
            inv(serde_json::json!({
                "path": "../../etc/hosts",
                "oldString": "a",
                "newString": "b"
            })),
        )
        .await;
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::PathOutsideWorkspace));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn edit_unique_hit_replaces_one() {
        let ws = temp_workspace("edit-unique");
        handle_write_file(
            ws.clone(),
            inv(serde_json::json!({ "path": "f.txt", "content": "alpha beta gamma" })),
        )
        .await;
        let out = handle_edit_file(
            ws.clone(),
            inv(serde_json::json!({ "path": "f.txt", "oldString": "beta", "newString": "BETA" })),
        )
        .await;
        assert!(!out.is_error);
        assert_eq!(out.output_summary["replaced"], 1);
        let content = std::fs::read_to_string(ws.join("f.txt")).unwrap();
        assert_eq!(content, "alpha BETA gamma");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn edit_no_hit_errors() {
        let ws = temp_workspace("edit-nohit");
        handle_write_file(
            ws.clone(),
            inv(serde_json::json!({ "path": "f.txt", "content": "abc" })),
        )
        .await;
        let out = handle_edit_file(
            ws.clone(),
            inv(serde_json::json!({ "path": "f.txt", "oldString": "zzz", "newString": "x" })),
        )
        .await;
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::InvalidInput));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn edit_non_unique_without_replace_all_errors() {
        let ws = temp_workspace("edit-nonunique");
        handle_write_file(
            ws.clone(),
            inv(serde_json::json!({ "path": "f.txt", "content": "x x x" })),
        )
        .await;
        // not unique, replaceAll defaults false → error
        let out = handle_edit_file(
            ws.clone(),
            inv(serde_json::json!({ "path": "f.txt", "oldString": "x", "newString": "y" })),
        )
        .await;
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::InvalidInput));

        // replaceAll true → replaces all 3
        let out = handle_edit_file(
            ws.clone(),
            inv(serde_json::json!({ "path": "f.txt", "oldString": "x", "newString": "y", "replaceAll": true })),
        )
        .await;
        assert!(!out.is_error);
        assert_eq!(out.output_summary["replaced"], 3);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn read_arbitrary_path_ok() {
        let ws = temp_workspace("read-arbitrary");
        // write a file OUTSIDE workspace directly (read is unrestricted)
        let outside = std::env::temp_dir().join(format!("gangzi-read-outside-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&outside, "outside content").unwrap();
        let out = handle_read_file(ws.clone(), inv(serde_json::json!({ "path": outside.to_str().unwrap() }))).await;
        assert!(!out.is_error);
        assert_eq!(out.output_summary["content"], "outside content");
        std::fs::remove_file(&outside).ok();
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn read_missing_path_not_found() {
        let ws = temp_workspace("read-missing");
        let out = handle_read_file(ws.clone(), inv(serde_json::json!({ "path": "no/such/file.txt" }))).await;
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::NotFound));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn read_large_file_truncated() {
        let ws = temp_workspace("read-large");
        let big = "a".repeat(READ_MAX_BYTES + 5000);
        handle_write_file(ws.clone(), inv(serde_json::json!({ "path": "big.txt", "content": big }))).await;
        let target = ws.join("big.txt");
        let out = handle_read_file(ws.clone(), inv(serde_json::json!({ "path": target.to_str().unwrap() }))).await;
        assert!(!out.is_error);
        assert_eq!(out.output_summary["truncated"], true);
        assert!(out.output_summary["content"].as_str().unwrap().len() <= READ_MAX_BYTES);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn bash_echo_succeeds() {
        let ws = temp_workspace("bash-echo");
        let out = handle_run_bash(ws.clone(), inv(serde_json::json!({ "command": "echo hello" }))).await;
        assert!(!out.is_error, "{:?}", out.output_summary);
        assert!(out.output_summary["stdout"].as_str().unwrap().contains("hello"));
        assert_eq!(out.output_summary["exitCode"], 0);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn bash_false_nonzero_exit() {
        let ws = temp_workspace("bash-false");
        let out = handle_run_bash(ws.clone(), inv(serde_json::json!({ "command": "false" }))).await;
        assert!(!out.is_error); // non-zero exit is not a tool error
        assert_ne!(out.output_summary["exitCode"], 0);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn bash_dangerous_rejected() {
        let ws = temp_workspace("bash-danger");
        // create a marker file the dangerous command would (if executed) delete the dir
        let out = handle_run_bash(ws.clone(), inv(serde_json::json!({ "command": "rm -rf /" }))).await;
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::CommandRejected));
        // workspace must still exist (command not executed)
        assert!(ws.exists());
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn bash_cwd_defaults_to_workspace() {
        let ws = temp_workspace("bash-cwd");
        let canon = std::fs::canonicalize(&ws).unwrap();
        let out = handle_run_bash(ws.clone(), inv(serde_json::json!({ "command": "pwd" }))).await;
        assert!(!out.is_error);
        let pwd = out.output_summary["stdout"].as_str().unwrap().trim();
        // canonicalize both sides to handle /var ↔ /private/var on macOS
        assert_eq!(std::fs::canonicalize(pwd).unwrap(), canon);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn bash_timeout_kills() {
        let ws = temp_workspace("bash-timeout");
        let out = handle_run_bash(
            ws.clone(),
            inv(serde_json::json!({ "command": "sleep 5", "timeoutMs": 100 })),
        )
        .await;
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::ToolTimeout));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn register_local_tools_registers_four() {
        let ws = temp_workspace("register");
        let registry = ToolRegistry::new_without_persist();
        register_local_tools(&registry, ws.clone()).unwrap();
        assert!(registry.has_tool("read_file"));
        assert!(registry.has_tool("write_file"));
        assert!(registry.has_tool("edit_file"));
        assert!(registry.has_tool("run_bash"));
        assert_eq!(registry.list_tools().len(), 4);
        std::fs::remove_dir_all(&ws).ok();
    }
}
