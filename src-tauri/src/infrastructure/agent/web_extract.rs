//! `web_extract` —— 抓网页正文（Infra 默认工具，**无需 key**）。
//!
//! Spec: docs/design/agent-infra-module.md §3.6 `web_extract`
//!
//! 参考 hermes-agent `web_extract`（search 给 URL+摘要，extract 才读得到原文）。配合 `web_search` +
//! fork 子 agent 构成完整研究链：搜 → 读正文 → 综合。读 URL → reqwest 抓 HTML → `scraper` 抽
//! main content（readability-lite：取 article/main/body 内的 p/h/li 文本，丢 nav/script）→ 截断。
//! 多 URL 并行、单条容错。前端不直接发外部 HTTP，全部走 Rust（架构红线）。
//!
//! MVP 局限：只处理 HTML；PDF（年报 / arxiv）暂不支持（content-type 非 HTML → 回 error，后续补）。

use crate::domain::agent::{SideEffect, ToolSpec};
use crate::domain::shared::ErrorCode;
use crate::infrastructure::agent::tool_registry::{
    FnToolHandler, RegisterError, ToolHandler, ToolHandlerFuture, ToolHandlerOutput, ToolInvocation,
    ToolRegistry,
};
use scraper::{Html, Selector};
use serde_json::json;

const MAX_URLS: usize = 5;
const MAX_CHARS_PER_PAGE: usize = 8000;
const EXTRACT_TIMEOUT_MS: u64 = 25_000;
const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

/// 注册 `web_extract` 工具（Infra 默认，无 key；bootstrap 注入一个 reqwest client）。
pub fn register_web_extract_tool(
    registry: &ToolRegistry,
    client: reqwest::Client,
) -> Result<(), RegisterError> {
    let spec = ToolSpec::new(
        "web_extract",
        "读网页正文：给一组 URL（≤5），抓回每页的标题 + 正文文本（已去导航/脚本/广告）。\
         web_search 只给摘要，要读研报 / 年报 / 文章原文用本工具。多 URL 并行、单条失败不影响其余。\
         只读。（当前仅 HTML；PDF 暂不支持。）",
        json!({
            "type": "object",
            "properties": {
                "urls": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "要读正文的页面 URL，最多 5 个",
                    "maxItems": MAX_URLS
                }
            },
            "required": ["urls"]
        }),
        vec![r#"<use_tool name="web_extract">{"urls":["https://www.example.com/report"]}</use_tool>"#.into()],
        EXTRACT_TIMEOUT_MS,
        SideEffect::None,
    );

    let handler: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(move |inv: ToolInvocation| {
        let client = client.clone();
        Box::pin(async move {
            let urls: Vec<String> = inv
                .input
                .get("urls")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            if urls.is_empty() {
                return ToolHandlerOutput::err(
                    json!({ "reason": "invalid_input", "message": "urls 不能为空" }),
                    ErrorCode::InvalidInput,
                );
            }
            let capped: Vec<String> = urls.into_iter().take(MAX_URLS).collect();
            let results = futures_util::future::join_all(
                capped.iter().map(|u| extract_one(&client, u)),
            )
            .await;
            ToolHandlerOutput::ok(json!({ "results": results }))
        }) as ToolHandlerFuture
    }));

    registry.register_tool(spec, handler)
}

use std::sync::Arc;

/// 抓单个 URL 的正文，归一成 `{url, title, content}` 或 `{url, error}`（容错，不抛）。
async fn extract_one(client: &reqwest::Client, url: &str) -> serde_json::Value {
    let resp = match client.get(url).header("User-Agent", UA).send().await {
        Ok(r) => r,
        Err(e) => return json!({ "url": url, "error": format!("fetch failed: {e}") }),
    };
    if !resp.status().is_success() {
        return json!({ "url": url, "error": format!("http {}", resp.status()) });
    }
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !ct.is_empty() && !ct.contains("html") && !ct.contains("xml") && !ct.contains("text/") {
        return json!({ "url": url, "error": format!("非 HTML 内容（{ct}），暂不支持（如 PDF）") });
    }
    let html = match resp.text().await {
        Ok(t) => t,
        Err(e) => return json!({ "url": url, "error": format!("read body failed: {e}") }),
    };
    let (title, content) = parse_main_content(&html);
    if content.trim().is_empty() {
        return json!({ "url": url, "title": title, "error": "未抽到正文（页面可能是 JS 渲染）" });
    }
    json!({ "url": url, "title": title, "content": content })
}

/// readability-lite：标题 + 取 article/main/body 内的 p/h/li/blockquote 文本（去 nav/script 噪声）。
/// scraper 的 Html 非 Send → 只在此同步函数里建+丢，不跨 await。
fn parse_main_content(html: &str) -> (String, String) {
    let doc = Html::parse_document(html);

    // 标题：<title> 优先，回退 og:title / 首个 h1。
    let title = doc
        .select(&Selector::parse("title").unwrap())
        .next()
        .map(|e| e.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            doc.select(&Selector::parse(r#"meta[property="og:title"]"#).unwrap())
                .next()
                .and_then(|e| e.value().attr("content").map(|s| s.trim().to_string()))
        })
        .unwrap_or_default();

    // 主容器：article / main / [role=main] 优先，回退 body。
    let container = ["article", "main", "[role=main]", "body"]
        .iter()
        .find_map(|sel| Selector::parse(sel).ok().and_then(|s| doc.select(&s).next()))
        .unwrap_or_else(|| doc.root_element());

    // 容器内取内容承载节点的文本（天然避开 script/style/nav 的非文本噪声）。
    let block_sel = Selector::parse("p, h1, h2, h3, h4, li, blockquote, td").unwrap();
    let mut out = String::new();
    for el in container.select(&block_sel) {
        let t = el.text().collect::<String>();
        let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.len() >= 2 {
            out.push_str(&t);
            out.push('\n');
            if out.chars().count() >= MAX_CHARS_PER_PAGE {
                break;
            }
        }
    }
    let content: String = out.trim().chars().take(MAX_CHARS_PER_PAGE).collect();
    (title, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_title_and_main_content_dropping_noise() {
        let html = r#"
            <html><head><title>贵州茅台产业链分析</title></head>
            <body>
              <nav><a href="/">导航不要</a></nav>
              <script>var x = "脚本不要";</script>
              <article>
                <h1>核心业务</h1>
                <p>公司主营高端白酒，处于白酒产业链中游偏上。</p>
                <p>上游是高粱小麦包装，下游是经销商与终端。</p>
              </article>
              <footer>页脚不要</footer>
            </body></html>"#;
        let (title, content) = parse_main_content(html);
        assert_eq!(title, "贵州茅台产业链分析");
        assert!(content.contains("核心业务"));
        assert!(content.contains("白酒产业链中游"));
        assert!(content.contains("经销商"));
        // 噪声不进正文
        assert!(!content.contains("导航不要"));
        assert!(!content.contains("脚本不要"));
        assert!(!content.contains("页脚不要"));
    }

    #[test]
    fn empty_body_yields_empty_content() {
        let (_t, c) = parse_main_content("<html><body></body></html>");
        assert!(c.trim().is_empty());
    }

    #[tokio::test]
    async fn extract_tool_rejects_empty_urls() {
        let registry = ToolRegistry::new_without_persist();
        register_web_extract_tool(&registry, reqwest::Client::new()).unwrap();
        let res = registry
            .dispatch_tool_call("run", "tc".into(), "web_extract", json!({ "urls": [] }))
            .await
            .unwrap();
        assert!(res.is_error);
        assert_eq!(res.error_code, Some(ErrorCode::InvalidInput));
    }

    // live：抓一个稳定页面验证整链（fetch + 抽正文）。
    #[tokio::test]
    #[ignore = "live: 真实抓取 example.com"]
    async fn extract_live_example_com() {
        let registry = ToolRegistry::new_without_persist();
        register_web_extract_tool(
            &registry,
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .unwrap(),
        )
        .unwrap();
        let res = registry
            .dispatch_tool_call(
                "run",
                "tc".into(),
                "web_extract",
                json!({ "urls": ["https://example.com"] }),
            )
            .await
            .unwrap();
        assert!(!res.is_error, "{:?}", res.output_summary);
        let first = &res.output_summary["results"][0];
        println!("title={} content={:?}", first["title"], first["content"]);
        assert!(
            first["content"].as_str().unwrap_or("").contains("Example Domain")
                || first["title"].as_str().unwrap_or("").contains("Example"),
            "应抽到 example.com 的正文/标题"
        );
    }
}
