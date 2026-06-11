//! `gangzi` —— GangZi 终端的只读查询 CLI（瘦客户端）。
//!
//! Spec: docs/design/cli-module.md
//!
//! 三段式：解析参数（adapters::cli::args，纯函数）→ 发本地 HTTP（std TcpStream，零额外依赖、
//! 进程内无任何业务 I/O）→ 渲染（JSON 透传 / table）。app 未运行 → `app_not_running` 退出码 3。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::ExitCode;
use std::time::Duration;

use gangzi_terminal::adapters::cli::args::{parse, CliRequest, OutputFormat};
use gangzi_terminal::adapters::cli::render::render_table;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let req = match parse(&argv) {
        Ok(r) => r,
        Err(msg) => {
            // --help 也走这条：usage 打 stdout、退出 0；真错误打 stderr、退出 2。
            if msg.contains("只读查询 CLI") && (argv.is_empty() || argv[0].starts_with("--h") || argv[0] == "help" || argv[0] == "-h") {
                println!("{msg}");
                return ExitCode::SUCCESS;
            }
            eprintln!(
                "{}",
                serde_json::json!({ "error": { "code": "invalid_input", "message": msg } })
            );
            return ExitCode::from(2);
        }
    };

    let port = match resolve_port() {
        Ok(p) => p,
        Err(msg) => return app_not_running(&msg),
    };

    let body = req.body.to_string();
    let raw = format!(
        "POST /v1/{} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        req.route,
        body.len(),
        body
    );
    let resp = match http_roundtrip(port, raw.as_bytes()) {
        Ok(r) => r,
        Err(msg) => return app_not_running(&msg),
    };

    let (status, payload) = match split_response(&resp) {
        Some(x) => x,
        None => return app_not_running("端点响应不可解析（app 版本不匹配？）"),
    };
    let v: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "{}",
                serde_json::json!({ "error": { "code": "parse_error", "message": format!("响应非 JSON: {e}") } })
            );
            return ExitCode::from(1);
        }
    };

    if status != 200 {
        // spec §5：透传 app 侧 ErrorCode，stderr + 非零退出。
        eprintln!("{v}");
        return ExitCode::from(1);
    }
    print_ok(&req, &v);
    ExitCode::SUCCESS
}

fn print_ok(req: &CliRequest, v: &serde_json::Value) {
    match req.format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(v).unwrap_or_default()),
        OutputFormat::Table => println!("{}", render_table(req.command, v)),
    }
}

fn app_not_running(detail: &str) -> ExitCode {
    eprintln!(
        "{}",
        serde_json::json!({
            "error": {
                "code": "app_not_running",
                "message": format!("GangZi app 未运行或端点不可达：{detail}（请先启动 app）")
            }
        })
    );
    ExitCode::from(3)
}

/// 端点发现（spec §2）：`$GANGZI_CLI_PORT` 显式覆盖 → `<appData>/cli.port`（app 启动写入）。
fn resolve_port() -> Result<u16, String> {
    if let Ok(p) = std::env::var("GANGZI_CLI_PORT") {
        return p.trim().parse().map_err(|_| format!("GANGZI_CLI_PORT 非法: {p}"));
    }
    let path = portfile_path().ok_or("无法定位 appData 目录")?;
    let s = std::fs::read_to_string(&path)
        .map_err(|_| format!("portfile 不存在: {}", path.display()))?;
    s.trim()
        .parse()
        .map_err(|_| format!("portfile 内容非法: {}", path.display()))
}

/// `<appData>/cli.port`，与 app 侧 `app_data_dir().join("cli.port")` 一致（identifier com.gangzi.terminal）。
fn portfile_path() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        Some(
            std::path::PathBuf::from(home)
                .join("Library/Application Support/com.gangzi.terminal/cli.port"),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
            })?;
        Some(base.join("com.gangzi.terminal/cli.port"))
    }
}

/// 同步 HTTP roundtrip（Connection: close → 读到 EOF）。
fn http_roundtrip(port: u16, request: &[u8]) -> Result<Vec<u8>, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("connect 127.0.0.1:{port} 失败: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .and_then(|_| stream.set_write_timeout(Some(Duration::from_secs(10))))
        .map_err(|e| format!("socket 配置失败: {e}"))?;
    stream
        .write_all(request)
        .map_err(|e| format!("请求发送失败: {e}"))?;
    let mut out = Vec::new();
    stream
        .read_to_end(&mut out)
        .map_err(|e| format!("响应读取失败: {e}"))?;
    Ok(out)
}

/// 拆响应：返回 (status, body&str)。
fn split_response(raw: &[u8]) -> Option<(u16, &str)> {
    let text = std::str::from_utf8(raw).ok()?;
    let status: u16 = text.split_whitespace().nth(1)?.parse().ok()?;
    let body = &text[text.find("\r\n\r\n")? + 4..];
    Some((status, body))
}
