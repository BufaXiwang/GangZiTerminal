//! web_search 搜索源实现：**免费、无 key、HTML 抓取**的源。
//!
//! - DuckDuckGo（html + lite 双端点）：国际内容。
//! - 搜狗（sogou.com/web）：中文 / A 股财经内容主力（实测「宁德时代 2025 一季报」直接命中
//!   东财 / 腾讯财经 / 新浪财经），免费无 key。结果链接是 `/link?url=...` 跳转包装——
//!   并行轻量解包（跳转页仅 ~300B，内含真实 URL），解不开保留包装链接（web_extract 会跟随跳转）。
//! - 不做的源（2026-06-11 实测）：百度（无头抓取直接弹 wappass 图形验证码）；
//!   **Bing 中国版**（cn.bing.com 无 cookie SERP 有「地名前缀实体回退」bug：「贵州茅台」→贵州省、
//!   「宁德时代」→宁德市旅游，对财经查询是主动误导，弃用）；360（跳转包装且收益边际）。
//! - 需 key 的源（Jina / Brave / Bocha / Tavily）按用户要求已移除——只留免费版本。
//!
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

// ───────────────────────────── 搜狗（免费、无 key、HTML 抓取；中文财经主力）
//
// `www.sogou.com/web?query=...`：无 JS 的 SSR 结果页。结果块 `div.vrwrap` / `div.rb`：
// `h3 a` 是标题+链接（高亮词在 `<em>` 内，text() 自动拼接）。链接是 `/link?url=...`
// 跳转包装——跳转页只有 ~300B（`window.location.replace("真实URL")`），并行 GET 解包，
// 失败保留包装链接（web_extract 跟随 meta refresh 也能读）。

pub struct Sogou {
    client: reqwest::Client,
}

impl Sogou {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl WebSearchProvider for Sogou {
    fn name(&self) -> &str {
        "sogou"
    }

    async fn search(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<WebSearchResult>, WebSearchError> {
        let resp = self
            .client
            .get("https://www.sogou.com/web")
            .query(&[("query", query)])
            .header("User-Agent", UA)
            .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.5")
            .send()
            .await
            .map_err(http_err)?;
        if !resp.status().is_success() {
            return Err(WebSearchError::Http(format!("sogou status {}", resp.status())));
        }
        let body = resp.text().await.map_err(http_err)?;
        if body.contains("antispider") {
            return Err(WebSearchError::Http("sogou antispider challenge".into()));
        }
        // 同步解析（scraper::Html 非 Send，不跨 await），再并行解包跳转链接。
        let rows = parse_sogou(&body, max_results);
        let futs = rows.into_iter().map(|mut r| {
            let client = self.client.clone();
            async move {
                if r.url.starts_with("https://www.sogou.com/link") {
                    if let Some(real) = resolve_sogou_link(&client, &r.url).await {
                        r.url = real;
                    }
                }
                r
            }
        });
        Ok(futures_util::future::join_all(futs).await)
    }
}

/// 解析 sogou.com/web 的 SSR 结果页（`div.vrwrap` / `div.rb` 块）。
fn parse_sogou(html: &str, max: usize) -> Vec<WebSearchResult> {
    let doc = Html::parse_document(html);
    let (Ok(row_sel), Ok(a_sel), Ok(snip_sel)) = (
        Selector::parse("div.vrwrap, div.rb"),
        Selector::parse("h3 a"),
        Selector::parse(".space-txt, .str_info, .ft, .text-layout"),
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for row in doc.select(&row_sel) {
        let Some(a) = row.select(&a_sel).next() else {
            continue;
        };
        let href = a.value().attr("href").unwrap_or("");
        let url = if let Some(rel) = href.strip_prefix('/') {
            format!("https://www.sogou.com/{rel}")
        } else {
            href.to_string()
        };
        let title = a.text().collect::<String>().trim().to_string();
        if url.is_empty() || !url.starts_with("http") || title.is_empty() {
            continue;
        }
        let snippet = row
            .select(&snip_sel)
            .map(|s| {
                let t = s.text().collect::<String>();
                t.split_whitespace().collect::<Vec<_>>().join(" ")
            })
            .find(|t| !t.is_empty())
            .map(|t| t.chars().take(300).collect::<String>())
            .unwrap_or_default();
        out.push(WebSearchResult {
            title,
            url,
            snippet,
            source: "sogou".into(),
        });
        if out.len() >= max {
            break;
        }
    }
    out
}

/// 解包搜狗跳转链接：GET `/link?url=...`（~300B）→ 取 `window.location.replace("真实URL")`
/// 或 `URL='真实URL'`（noscript meta refresh）。失败 → None（caller 保留包装链接）。
async fn resolve_sogou_link(client: &reqwest::Client, link: &str) -> Option<String> {
    let resp = client
        .get(link)
        .header("User-Agent", UA)
        .header("Referer", "https://www.sogou.com/web")
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    let body = resp.text().await.ok()?;
    extract_sogou_target(&body)
}

/// 从跳转页正文提取真实 URL（纯文本扫描，无正则依赖）。
fn extract_sogou_target(body: &str) -> Option<String> {
    for (marker, end) in [("location.replace(\"", "\""), ("URL='", "'")] {
        if let Some(pos) = body.find(marker) {
            let rest = &body[pos + marker.len()..];
            if let Some(e) = rest.find(end) {
                let url = &rest[..e];
                if url.starts_with("http") {
                    return Some(url.to_string());
                }
            }
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    /// sogou.com/web SSR 结果页的最小 fixture（结构取自 2026-06-11 实抓页面：
    /// `div.vrwrap` 块、`h3.vr-title > a` 标题（高亮词在 `<em>` + 注释标记）、`.space-txt` 摘要）。
    const SOGOU_FIXTURE: &str = r#"<html><body>
      <div class="vrwrap">
        <h3 class="vr-title"><a href="/link?url=hedJjaC291Pabc..">
          <em><!--red_beg-->宁德时代2025年一季报<!--red_end--></em>业绩高增长|宁...</a></h3>
        <div class="text-layout"><div class="fz-mid space-txt">新华财经上海4月14日电
          宁德时代发布2025年一季报，净利润139.63亿元</div></div>
      </div>
      <div class="rb">
        <h3><a href="https://finance.qq.com/a/123.htm">宁德时代一季度净利同比增32.9%_腾讯新闻</a></h3>
        <div class="ft">产销两旺驱动开门红</div>
      </div>
      <div class="vrwrap"><h3><a href="javascript:void(0)">无效链接被跳过</a></h3></div>
    </body></html>"#;

    #[test]
    fn parse_sogou_extracts_rows_resolves_relative_and_skips_non_http() {
        let rows = parse_sogou(SOGOU_FIXTURE, 8);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].source, "sogou");
        // 相对 /link 补成绝对（后续异步解包真实 URL）。
        assert!(rows[0].url.starts_with("https://www.sogou.com/link?url="));
        // 标题拼接 <em> 高亮（注释标记不进 text）。
        assert!(rows[0].title.contains("宁德时代2025年一季报"), "{}", rows[0].title);
        assert!(rows[0].snippet.contains("139.63"), "{}", rows[0].snippet);
        // 直链原样 + .ft 摘要。
        assert_eq!(rows[1].url, "https://finance.qq.com/a/123.htm");
        assert!(rows[1].snippet.contains("产销两旺"));
        // max 截断
        assert_eq!(parse_sogou(SOGOU_FIXTURE, 1).len(), 1);
    }

    #[test]
    fn extract_sogou_target_handles_replace_and_meta_refresh() {
        // window.location.replace 形态（实抓跳转页）。
        let js = r#"<meta content="always" name="referrer"><script>window.location.replace("https://finance.sina.com.cn/doc-abc.shtml")</script>"#;
        assert_eq!(
            extract_sogou_target(js).as_deref(),
            Some("https://finance.sina.com.cn/doc-abc.shtml")
        );
        // noscript meta refresh 形态。
        let meta = r#"<noscript><META http-equiv="refresh" content="0;URL='https://example.com/x'"></noscript>"#;
        assert_eq!(extract_sogou_target(meta).as_deref(), Some("https://example.com/x"));
        // 解不出 → None。
        assert_eq!(extract_sogou_target("<html>nothing</html>"), None);
    }
}
