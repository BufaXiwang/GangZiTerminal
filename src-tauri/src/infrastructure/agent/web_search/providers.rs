//! web_search 各源实现：DuckDuckGo（HTML 抓取，免费无 key）/ Jina（免费）/ 博查 Bocha / Tavily。
//!
//! 每个源把自家响应归一成 `WebSearchResult`。wire format 是 infra 细节（不入 spec）。

use super::{WebSearchError, WebSearchProvider, WebSearchResult, UA};
use scraper::{Html, Selector};
use serde_json::json;

fn http_err(e: reqwest::Error) -> WebSearchError {
    WebSearchError::Http(e.to_string())
}

// ───────────────────────────── DuckDuckGo（免费、无 key、HTML 抓取）

pub struct DuckDuckGo {
    client: reqwest::Client,
}

impl DuckDuckGo {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
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
        // html.duckduckgo.com/html/ 是无 JS 的 SSR 结果页，便于抓取。
        let resp = self
            .client
            .post("https://html.duckduckgo.com/html/")
            .header("User-Agent", UA)
            .form(&[("q", query)])
            .send()
            .await
            .map_err(http_err)?;
        if !resp.status().is_success() {
            return Err(WebSearchError::Http(format!("ddg status {}", resp.status())));
        }
        let html = resp.text().await.map_err(http_err)?;
        // scraper 的 Html 非 Send：在此同步函数里建+丢，不跨 await。
        Ok(parse_ddg(&html, max_results))
    }
}

fn parse_ddg(html: &str, max: usize) -> Vec<WebSearchResult> {
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

// ───────────────────────────── Jina（s.jina.ai，免费；可选 key 提额）

pub struct Jina {
    client: reqwest::Client,
    key: String,
}

impl Jina {
    pub fn new(client: reqwest::Client, key: String) -> Self {
        Self { client, key }
    }
}

#[async_trait::async_trait]
impl WebSearchProvider for Jina {
    fn name(&self) -> &str {
        "jina"
    }

    async fn search(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<WebSearchResult>, WebSearchError> {
        // s.jina.ai/<query>；Accept: application/json 拿结构化结果。
        let mut url = reqwest::Url::parse("https://s.jina.ai/")
            .map_err(|e| WebSearchError::Parse(e.to_string()))?;
        url.path_segments_mut()
            .map_err(|_| WebSearchError::Parse("bad base url".into()))?
            .push(query);
        let resp = self
            .client
            .get(url)
            .header("Accept", "application/json")
            .header("User-Agent", UA)
            .header("Authorization", format!("Bearer {}", self.key))
            .send()
            .await
            .map_err(http_err)?;
        if resp.status() == 401 || resp.status() == 403 {
            return Err(WebSearchError::Auth(format!("jina {}", resp.status())));
        }
        if !resp.status().is_success() {
            return Err(WebSearchError::Http(format!("jina status {}", resp.status())));
        }
        let v: serde_json::Value = resp.json().await.map_err(http_err)?;
        let arr = v
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| WebSearchError::Parse("jina: no data[]".into()))?;
        Ok(arr
            .iter()
            .take(max_results)
            .filter_map(|item| {
                let url = item.get("url").and_then(|x| x.as_str())?.to_string();
                let title = item
                    .get("title")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let snippet = item
                    .get("description")
                    .or_else(|| item.get("content"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .chars()
                    .take(400)
                    .collect();
                Some(WebSearchResult {
                    title,
                    url,
                    snippet,
                    source: "jina".into(),
                })
            })
            .collect())
    }
}

// ───────────────────────────── Brave Search（免费额度 2000/月，需 key，最稳的免费源）

pub struct Brave {
    client: reqwest::Client,
    key: String,
}

impl Brave {
    pub fn new(client: reqwest::Client, key: String) -> Self {
        Self { client, key }
    }
}

#[async_trait::async_trait]
impl WebSearchProvider for Brave {
    fn name(&self) -> &str {
        "brave"
    }

    async fn search(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<WebSearchResult>, WebSearchError> {
        let count = max_results.clamp(1, 20).to_string();
        let resp = self
            .client
            .get("https://api.search.brave.com/res/v1/web/search")
            .query(&[("q", query), ("count", count.as_str())])
            .header("Accept", "application/json")
            .header("X-Subscription-Token", &self.key)
            .send()
            .await
            .map_err(http_err)?;
        if resp.status() == 401 || resp.status() == 403 {
            return Err(WebSearchError::Auth(format!("brave {}", resp.status())));
        }
        if !resp.status().is_success() {
            return Err(WebSearchError::Http(format!("brave status {}", resp.status())));
        }
        let v: serde_json::Value = resp.json().await.map_err(http_err)?;
        let arr = v
            .pointer("/web/results")
            .and_then(|x| x.as_array())
            .ok_or_else(|| WebSearchError::Parse("brave: no web.results[]".into()))?;
        Ok(arr
            .iter()
            .take(max_results)
            .filter_map(|item| {
                let url = item.get("url").and_then(|x| x.as_str())?.to_string();
                let title = item
                    .get("title")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let snippet = item
                    .get("description")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .chars()
                    .take(400)
                    .collect();
                Some(WebSearchResult {
                    title,
                    url,
                    snippet,
                    source: "brave".into(),
                })
            })
            .collect())
    }
}

// ───────────────────────────── 博查 Bocha（中文最佳，需 key）

pub struct Bocha {
    client: reqwest::Client,
    key: String,
}

impl Bocha {
    pub fn new(client: reqwest::Client, key: String) -> Self {
        Self { client, key }
    }
}

#[async_trait::async_trait]
impl WebSearchProvider for Bocha {
    fn name(&self) -> &str {
        "bocha"
    }

    async fn search(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<WebSearchResult>, WebSearchError> {
        let resp = self
            .client
            .post("https://api.bochaai.com/v1/web-search")
            .header("Authorization", format!("Bearer {}", self.key))
            .json(&json!({ "query": query, "summary": true, "count": max_results }))
            .send()
            .await
            .map_err(http_err)?;
        if resp.status() == 401 || resp.status() == 403 {
            return Err(WebSearchError::Auth(format!("bocha {}", resp.status())));
        }
        if !resp.status().is_success() {
            return Err(WebSearchError::Http(format!("bocha status {}", resp.status())));
        }
        let v: serde_json::Value = resp.json().await.map_err(http_err)?;
        let arr = v
            .pointer("/data/webPages/value")
            .and_then(|x| x.as_array())
            .ok_or_else(|| WebSearchError::Parse("bocha: no data.webPages.value[]".into()))?;
        Ok(arr
            .iter()
            .take(max_results)
            .filter_map(|item| {
                let url = item.get("url").and_then(|x| x.as_str())?.to_string();
                let title = item
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let snippet = item
                    .get("summary")
                    .or_else(|| item.get("snippet"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .chars()
                    .take(400)
                    .collect();
                Some(WebSearchResult {
                    title,
                    url,
                    snippet,
                    source: "bocha".into(),
                })
            })
            .collect())
    }
}

// ───────────────────────────── Tavily（agent 友好，免费额度，需 key）

pub struct Tavily {
    client: reqwest::Client,
    key: String,
}

impl Tavily {
    pub fn new(client: reqwest::Client, key: String) -> Self {
        Self { client, key }
    }
}

#[async_trait::async_trait]
impl WebSearchProvider for Tavily {
    fn name(&self) -> &str {
        "tavily"
    }

    async fn search(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<WebSearchResult>, WebSearchError> {
        let resp = self
            .client
            .post("https://api.tavily.com/search")
            .header("Authorization", format!("Bearer {}", self.key))
            .json(&json!({ "query": query, "max_results": max_results }))
            .send()
            .await
            .map_err(http_err)?;
        if resp.status() == 401 || resp.status() == 403 {
            return Err(WebSearchError::Auth(format!("tavily {}", resp.status())));
        }
        if !resp.status().is_success() {
            return Err(WebSearchError::Http(format!("tavily status {}", resp.status())));
        }
        let v: serde_json::Value = resp.json().await.map_err(http_err)?;
        let arr = v
            .get("results")
            .and_then(|x| x.as_array())
            .ok_or_else(|| WebSearchError::Parse("tavily: no results[]".into()))?;
        Ok(arr
            .iter()
            .take(max_results)
            .filter_map(|item| {
                let url = item.get("url").and_then(|x| x.as_str())?.to_string();
                let title = item
                    .get("title")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let snippet = item
                    .get("content")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .chars()
                    .take(400)
                    .collect();
                Some(WebSearchResult {
                    title,
                    url,
                    snippet,
                    source: "tavily".into(),
                })
            })
            .collect())
    }
}
