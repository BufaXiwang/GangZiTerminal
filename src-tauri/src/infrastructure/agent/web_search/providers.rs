//! web_search 搜索源实现：当前只保留 **DuckDuckGo**（免费、无 key、HTML 抓取）。
//!
//! 需 key 的源（Jina / Brave / Bocha / Tavily）按用户要求已移除——只留免费版本。
//! 每个源把自家响应归一成 `WebSearchResult`。wire format 是 infra 细节（不入 spec）。

use super::{WebSearchError, WebSearchProvider, WebSearchResult, UA};
use scraper::{Html, Selector};

fn http_err(e: reqwest::Error) -> WebSearchError {
    WebSearchError::Http(e.to_string())
}

// ───────────────────────────── DuckDuckGo（免费、无 key、HTML 抓取）
//
// 两个无 JS 的 SSR 端点都试：html.duckduckgo.com/html/ 优先，失败/空再退到
// lite.duckduckgo.com/lite/。两者反爬强度不同（与发起 IP 有关）；本工具跑在用户本机
// Tauri 后端（住宅 IP），命中率高于数据中心 IP。聚合层对失败容错。

pub struct DuckDuckGo {
    client: reqwest::Client,
}

impl DuckDuckGo {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }

    async fn fetch(&self, url: &str, query: &str) -> Result<String, WebSearchError> {
        let resp = self
            .client
            .post(url)
            .header("User-Agent", UA)
            .form(&[("q", query)])
            .send()
            .await
            .map_err(http_err)?;
        if !resp.status().is_success() {
            return Err(WebSearchError::Http(format!("ddg {} status {}", url, resp.status())));
        }
        resp.text().await.map_err(http_err)
    }
}

#[async_trait::async_trait]
impl WebSearchProvider for DuckDuckGo {
    fn name(&self) -> &str {
        "duckduckgo"
    }

    async fn search(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<WebSearchResult>, WebSearchError> {
        // 先试 html（结果带 snippet），空了再退 lite（更轻、反爬阈值不同）。
        let html = self.fetch("https://html.duckduckgo.com/html/", query).await;
        if let Ok(body) = &html {
            // scraper 的 Html 非 Send：在同步函数里建+丢，不跨 await。
            let rows = parse_ddg_html(body, max_results);
            if !rows.is_empty() {
                return Ok(rows);
            }
        }
        let body = self.fetch("https://lite.duckduckgo.com/lite/", query).await?;
        let rows = parse_ddg_lite(&body, max_results);
        if rows.is_empty() {
            // 两端点都解析不出 → 把 html 的错误（若有）透出，便于诊断反爬。
            if let Err(e) = html {
                return Err(e);
            }
        }
        Ok(rows)
    }
}

/// 解析 html.duckduckgo.com/html/ 的 SSR 结果页（`div.result` 列表）。
fn parse_ddg_html(html: &str, max: usize) -> Vec<WebSearchResult> {
    let doc = Html::parse_document(html);
    let (Ok(row_sel), Ok(a_sel), Ok(snip_sel)) = (
        Selector::parse("div.result"),
        Selector::parse("a.result__a"),
        Selector::parse("a.result__snippet"),
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for row in doc.select(&row_sel) {
        let Some(a) = row.select(&a_sel).next() else {
            continue;
        };
        let href = a.value().attr("href").unwrap_or("");
        let url = ddg_unwrap(href);
        let title = a.text().collect::<String>().trim().to_string();
        if url.is_empty() || title.is_empty() {
            continue;
        }
        let snippet = row
            .select(&snip_sel)
            .next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        out.push(WebSearchResult {
            title,
            url,
            snippet,
            source: "duckduckgo".into(),
        });
        if out.len() >= max {
            break;
        }
    }
    out
}

/// 解析 lite.duckduckgo.com/lite/ 的表格布局：结果链接是 `a.result-link`，
/// 紧随其后的 `td.result-snippet` 是摘要。
fn parse_ddg_lite(html: &str, max: usize) -> Vec<WebSearchResult> {
    let doc = Html::parse_document(html);
    let (Ok(a_sel), Ok(snip_sel)) = (
        Selector::parse("a.result-link"),
        Selector::parse("td.result-snippet"),
    ) else {
        return Vec::new();
    };
    let snippets: Vec<String> = doc
        .select(&snip_sel)
        .map(|s| s.text().collect::<String>().trim().to_string())
        .collect();
    let mut out = Vec::new();
    for (i, a) in doc.select(&a_sel).enumerate() {
        let href = a.value().attr("href").unwrap_or("");
        let url = ddg_unwrap(href);
        let title = a.text().collect::<String>().trim().to_string();
        if url.is_empty() || title.is_empty() {
            continue;
        }
        out.push(WebSearchResult {
            title,
            url,
            snippet: snippets.get(i).cloned().unwrap_or_default(),
            source: "duckduckgo".into(),
        });
        if out.len() >= max {
            break;
        }
    }
    out
}

/// DDG href 形如 `//duckduckgo.com/l/?uddg=<url-encoded>&rut=...`，解出真实 URL。
fn ddg_unwrap(href: &str) -> String {
    if let Some(pos) = href.find("uddg=") {
        let rest = &href[pos + 5..];
        let enc = rest.split('&').next().unwrap_or(rest);
        return percent_decode(enc);
    }
    if let Some(stripped) = href.strip_prefix("//") {
        return format!("https://{stripped}");
    }
    href.to_string()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(b[i]);
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
