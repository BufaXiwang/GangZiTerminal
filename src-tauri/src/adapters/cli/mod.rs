//! CLI 本地只读端点（app 侧）。
//!
//! Spec: docs/design/cli-module.md §2 瘦客户端机制 / §3 命令集 / §5 错误
//!
//! - app 启动时绑 `127.0.0.1:0`（随机端口，只绑本机），把端口写进 `<appData>/cli.port`；
//!   `gangzi` CLI 读 portfile 发现端点。app 没跑 → 连不上 → CLI 报 `app_not_running`。
//! - 路由全部**只读**，复用 agent_runtime 的三个 Gateway `fetch`（与 Agent 读 tool 字面同源，
//!   spec §1「CLI 输出的数据和 GUI 看到的一致」）：
//!     POST /v1/quotes   → QuotesGateway::fetch（tsCodes → fetch_data / scan → scan_market）
//!     POST /v1/news     → NewsGateway::fetch（FetchNewsRequest）
//!     POST /v1/account  → AccountGateway::fetch（FetchAccountRequest，只读）
//!     GET  /v1/health   → {"ok":true}
//!   **无任何写路由**（operate / watchlist / strategy 在 CLI 层即不可达，spec §3 只读边界）。
//! - HTTP 实现是手写最小 HTTP/1.1（只支持上述路由、`Connection: close`）——只服务本机 CLI，
//!   不是通用 server；端点形态属 Spec-anchored 实现细节。

pub mod args;
pub mod render;

use std::sync::Arc;

use serde_json::{json, Value as JsonValue};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::domain::shared::ErrorCode;
use crate::pipeline::agent_runtime::gateway_impls::{
    AccountGatewayImpl, NewsGatewayImpl, QuotesGatewayImpl,
};
use crate::pipeline::agent_runtime::gateways::{
    AccountGateway, GatewayError, NewsGateway, QuotesGateway,
};
use crate::pipeline::account::service::AccountService;
use crate::pipeline::news::service::NewsService;
use crate::pipeline::quotes::service::QuotesService;

/// 请求体上限（防御：CLI 请求都是小 JSON）。
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// 请求头上限。
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// CLI 端点持有的只读 gateway 集（与 Agent 读 tool 同一实现）。
#[derive(Clone)]
pub struct CliGateways {
    quotes: Arc<dyn QuotesGateway>,
    news: Arc<dyn NewsGateway>,
    account: Arc<dyn AccountGateway>,
}

impl CliGateways {
    pub fn new(
        quotes: Arc<QuotesService>,
        news: Arc<NewsService>,
        account: Arc<AccountService>,
    ) -> Self {
        Self {
            quotes: Arc::new(QuotesGatewayImpl::new(quotes)),
            news: Arc::new(NewsGatewayImpl::new(news)),
            account: Arc::new(AccountGatewayImpl::new(account)),
        }
    }

    /// 测试注入桩 gateway。
    #[cfg(test)]
    pub fn from_parts(
        quotes: Arc<dyn QuotesGateway>,
        news: Arc<dyn NewsGateway>,
        account: Arc<dyn AccountGateway>,
    ) -> Self {
        Self { quotes, news, account }
    }
}

/// 路由分发（纯逻辑，hermetic 可测）：`(route, body) -> (http_status, json)`。
///
/// Spec §3：只映射只读 use-case；§5：错误以 `{"error":{"code","message"}}` 返回。
pub async fn dispatch(gw: &CliGateways, method: &str, route: &str, body: JsonValue) -> (u16, JsonValue) {
    if method == "GET" && route == "health" {
        return (200, json!({ "ok": true }));
    }
    if method != "POST" {
        return (
            405,
            error_json(ErrorCode::InvalidInput, "method not allowed (POST only)"),
        );
    }
    let result = match route {
        "quotes" => gw.quotes.fetch(body).await,
        "news" => gw.news.fetch(body).await,
        "account" => gw.account.fetch(body).await,
        _ => {
            return (
                404,
                error_json(ErrorCode::NotFound, format!("unknown route: /v1/{route}")),
            )
        }
    };
    match result {
        Ok(v) => (200, v),
        Err(GatewayError { code, message }) => {
            let status = match code {
                ErrorCode::InvalidInput | ErrorCode::ParseError => 400,
                ErrorCode::NotFound => 404,
                _ => 500,
            };
            (status, error_json(code, message))
        }
    }
}

fn error_json(code: ErrorCode, message: impl Into<String>) -> JsonValue {
    let code_str = serde_json::to_value(code)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "error".into());
    json!({ "error": { "code": code_str, "message": message.into() } })
}

/// 启动本地只读端点：绑 `127.0.0.1:0` → 端口写 `portfile` → accept loop。
///
/// 随 app 进程存亡；portfile 残留无害（CLI 连不上即报 `app_not_running`）。
pub async fn serve(gw: CliGateways, portfile: std::path::PathBuf) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    if let Some(parent) = portfile.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&portfile, port.to_string())?;
    tracing::info!(target: "cli.endpoint", port, portfile = %portfile.display(), "CLI read-only endpoint up");

    loop {
        let (stream, _peer) = listener.accept().await?;
        let gw = gw.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, &gw).await {
                tracing::debug!(target: "cli.endpoint", error = %e, "conn error");
            }
        });
    }
}

/// 单连接处理：读请求（最小 HTTP/1.1）→ dispatch → 写响应（Connection: close）。
async fn handle_conn(
    mut stream: tokio::net::TcpStream,
    gw: &CliGateways,
) -> std::io::Result<()> {
    // ---- 读到头部结束（\r\n\r\n），带上限 ----
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let head_end = loop {
        let mut chunk = [0u8; 2048];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(()); // 客户端关了
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return write_response(&mut stream, 431, &error_json(ErrorCode::InvalidInput, "headers too large")).await;
        }
    };

    // ---- 解析请求行 + Content-Length ----
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_uppercase();
    let path = parts.next().unwrap_or("");
    let content_length: usize = lines
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim().parse().ok())?
        })
        .next()
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return write_response(&mut stream, 413, &error_json(ErrorCode::InvalidInput, "body too large")).await;
    }

    // ---- 读 body 余量 ----
    let mut body_bytes = buf[head_end..].to_vec();
    while body_bytes.len() < content_length {
        let mut chunk = vec![0u8; (content_length - body_bytes.len()).min(64 * 1024)];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body_bytes.extend_from_slice(&chunk[..n]);
    }

    // ---- 路由：仅 /v1/<route> ----
    let route = match path.strip_prefix("/v1/") {
        Some(r) => r.split('?').next().unwrap_or("").to_string(),
        None => {
            return write_response(&mut stream, 404, &error_json(ErrorCode::NotFound, "unknown path (expect /v1/...)")).await;
        }
    };
    let body: JsonValue = if body_bytes.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(e) => {
                return write_response(&mut stream, 400, &error_json(ErrorCode::InvalidInput, format!("invalid JSON body: {e}"))).await;
            }
        }
    };

    let (status, payload) = dispatch(gw, &method, &route, body).await;
    write_response(&mut stream, status, &payload).await
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    payload: &JsonValue,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ───────────────────────── tests (hermetic) ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::agent_runtime::gateways::OperateOutcome;
    use async_trait::async_trait;

    struct StubQuotes;
    #[async_trait]
    impl QuotesGateway for StubQuotes {
        async fn fetch(&self, input: JsonValue) -> Result<JsonValue, GatewayError> {
            if input.get("scan").is_some() {
                return Ok(json!({ "kind": "scan" }));
            }
            if input.get("tsCodes").is_none() {
                return Err(GatewayError::new(ErrorCode::InvalidInput, "need tsCodes"));
            }
            Ok(json!({ "kind": "fetch_data", "echo": input }))
        }
    }
    struct StubNews;
    #[async_trait]
    impl NewsGateway for StubNews {
        async fn fetch(&self, _input: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({ "items": [] }))
        }
    }
    struct StubAccount;
    #[async_trait]
    impl AccountGateway for StubAccount {
        async fn fetch(&self, _input: JsonValue) -> Result<JsonValue, GatewayError> {
            Ok(json!({ "snapshot": { "cash": "1000000" } }))
        }
        async fn operate(&self, _: JsonValue, _: &str, _: &str) -> OperateOutcome {
            panic!("CLI 端点绝不能触达写路径（spec §3 只读边界）");
        }
        async fn update_watchlist(&self, _: JsonValue) -> Result<JsonValue, GatewayError> {
            panic!("CLI 端点绝不能触达写路径（spec §3 只读边界）");
        }
    }

    fn gw() -> CliGateways {
        CliGateways::from_parts(Arc::new(StubQuotes), Arc::new(StubNews), Arc::new(StubAccount))
    }

    #[tokio::test]
    async fn dispatch_routes_readonly_queries() {
        let g = gw();
        let (s, v) = dispatch(&g, "POST", "quotes", json!({"tsCodes":["600519.SH"]})).await;
        assert_eq!(s, 200);
        assert_eq!(v["kind"], "fetch_data");
        let (s, v) = dispatch(&g, "POST", "quotes", json!({"scan":{"limit":5}})).await;
        assert_eq!(s, 200);
        assert_eq!(v["kind"], "scan");
        let (s, _) = dispatch(&g, "POST", "news", json!({"query":"茅台"})).await;
        assert_eq!(s, 200);
        let (s, v) = dispatch(&g, "POST", "account", json!({"include":{"snapshot":true}})).await;
        assert_eq!(s, 200);
        assert_eq!(v["snapshot"]["cash"], "1000000");
    }

    #[tokio::test]
    async fn dispatch_health_and_unknown_route() {
        let g = gw();
        let (s, v) = dispatch(&g, "GET", "health", json!({})).await;
        assert_eq!(s, 200);
        assert_eq!(v["ok"], true);
        // spec §3：未登记的子命令 / 任何写语义路由不存在 → 404。
        for bad in ["operate", "trade", "watchlist", "strategy", "nope"] {
            let (s, v) = dispatch(&g, "POST", bad, json!({})).await;
            assert_eq!(s, 404, "route {bad} 必须不可达");
            assert!(v["error"]["code"].is_string());
        }
    }

    #[tokio::test]
    async fn dispatch_maps_gateway_error_to_status_and_error_json() {
        let g = gw();
        let (s, v) = dispatch(&g, "POST", "quotes", json!({})).await; // 无 tsCodes 无 scan → invalid_input
        assert_eq!(s, 400);
        assert_eq!(v["error"]["code"], "invalid_input");
        assert!(v["error"]["message"].as_str().unwrap().contains("tsCodes"));
    }

    /// 端到端（本机 socket）：serve → 真 TCP 请求 → 响应解析。验证手写 HTTP 层。
    #[tokio::test]
    async fn serve_end_to_end_over_tcp() {
        let dir = std::env::temp_dir().join(format!("gangzi-cli-{}", uuid::Uuid::new_v4()));
        let portfile = dir.join("cli.port");
        let g = gw();
        let pf = portfile.clone();
        tokio::spawn(async move {
            let _ = serve(g, pf).await;
        });
        // 等 portfile 出现。
        let mut port: Option<u16> = None;
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(&portfile) {
                port = s.trim().parse().ok();
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let port = port.expect("portfile written");

        use tokio::net::TcpStream;
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let body = r#"{"include":{"snapshot":true}}"#;
        let req = format!(
            "POST /v1/account HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).await.unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        let json_body = &text[text.find("\r\n\r\n").unwrap() + 4..];
        let v: JsonValue = serde_json::from_str(json_body).unwrap();
        assert_eq!(v["snapshot"]["cash"], "1000000");
        std::fs::remove_dir_all(&dir).ok();
    }
}
