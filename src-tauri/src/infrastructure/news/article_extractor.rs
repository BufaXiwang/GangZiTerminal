//! Article extractor — 按 canonical URL 抓正文 + 抽 main content。
//!
//! Spec: docs/design/references/news/article-extractor.md
//!
//! 默认限制：
//! - request timeout 10s
//! - max body size 2MB
//! - retry 1 次（暂未实现 retry，留 TODO）

use crate::domain::news::types::ArticleContent;
use crate::domain::shared::{ErrorCode, OccurredAt, WarningCode};
use chrono::Utc;
use reqwest::Client;
use scraper::{Html, Selector};
use std::time::Duration;

pub const ARTICLE_TIMEOUT_SECS: u64 = 10;
pub const ARTICLE_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// 太短的正文视为抽取失败（spec §抽取规则）。
pub const ARTICLE_MIN_CONTENT_CHARS: usize = 80;

/// 抽取结果。`error` 仅在彻底失败（fetch / parse）时填充，调用方据此映射到
/// `NewsFailure(stage="article")`。
///
/// Spec: news-module.md §5 failure code 表 — article stage 失败统一 code = `article_extract_failed`，
/// 细分原因放 `reason`，供 service 层写入 `NewsFailure.details.reason`。
pub struct ArticleExtractOutput {
    pub article: ArticleContent,
    /// `(code, reason, message)`：`code` 固定为 `ArticleExtractFailed`；`reason` 为细分原因
    /// （`network` / `timeout` / `too_short` / `unsupported_content_type` / `http_status`
    /// / `parse_error`），调用方写入 `NewsFailure.details.reason`。
    pub error: Option<ArticleExtractFailure>,
}

#[derive(Debug, Clone)]
pub struct ArticleExtractFailure {
    pub code: ErrorCode,
    pub reason: ArticleExtractReason,
    pub message: String,
}

/// article-stage 细分原因（写入 `NewsFailure.details.reason`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArticleExtractReason {
    Network,
    Timeout,
    TooShort,
    UnsupportedContentType,
    HttpStatus,
    ParseError,
}

impl ArticleExtractReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ArticleExtractReason::Network => "network",
            ArticleExtractReason::Timeout => "timeout",
            ArticleExtractReason::TooShort => "too_short",
            ArticleExtractReason::UnsupportedContentType => "unsupported_content_type",
            ArticleExtractReason::HttpStatus => "http_status",
            ArticleExtractReason::ParseError => "parse_error",
        }
    }
}

const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";

/// 按 source（NewsNow channel）选正文抽取策略。详见 references/news/article-strategies.md。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArticleStrategy {
    /// 快讯：标题即全文，不抓取。
    TitleIsContent,
    /// 财联社详情页 `__NEXT_DATA__` JSON 内嵌正文（cls-telegraph / cls-depth）。
    ClsNextData,
    /// 华尔街见闻 api-one JSON（wallstreetcn-quick / wallstreetcn，按 url 判 lives/articles）。
    WallstreetcnApi,
    /// 36 氪快讯页 `<meta name=description>`（36kr-quick）。
    Kr36Meta,
    /// 静态页 CSS selector（gelonghui / fastbull-news / sputniknewscn 各自 selector）。
    /// `strip` 是要从正文容器里剔除的 noisy 子树 selector（免责声明 / 反广告拦截提示等）。
    StaticSelector {
        selector: &'static str,
        strip: &'static [&'static str],
    },
    /// 早晨报静态 GBK 页 CSS selector（zaobao→zaochenbao）。
    ZaochenbaoStatic,
    /// 参考消息：正文在内联 JS 变量 `var contentTxt="…"`。
    CankaoInlineScript,
    /// 未登记源：通用 readability 兜底。
    Generic,
}

/// source 前缀 → 策略。新增渠道在此登记 + reference 文档同步。
pub fn strategy_for(source: &str) -> ArticleStrategy {
    match source {
        "newsnow:cls-telegraph" | "newsnow:cls-depth" => ArticleStrategy::ClsNextData,
        "newsnow:wallstreetcn-quick" | "newsnow:wallstreetcn" => ArticleStrategy::WallstreetcnApi,
        "newsnow:36kr-quick" => ArticleStrategy::Kr36Meta,
        "newsnow:gelonghui" => ArticleStrategy::StaticSelector {
            selector: "article.main-news.article-with-html",
            strip: &[],
        },
        // fastbull 正文容器尾部带 .risk_tips 免责声明 + 收藏/分享按钮文字，剔除。
        "newsnow:fastbull-news" => ArticleStrategy::StaticSelector {
            selector: ".news-detail-content",
            strip: &[".risk_tips"],
        },
        "newsnow:sputniknewscn" => ArticleStrategy::StaticSelector {
            selector: ".article__body",
            strip: &[],
        },
        "newsnow:cankaoxiaoxi" => ArticleStrategy::CankaoInlineScript,
        "newsnow:zaobao" => ArticleStrategy::ZaochenbaoStatic,
        "newsnow:jin10" => ArticleStrategy::TitleIsContent,
        _ => ArticleStrategy::Generic,
    }
}

pub struct ArticleExtractor {
    client: Client,
}

impl ArticleExtractor {
    pub fn new() -> reqwest::Result<Self> {
        let client = Client::builder()
            .user_agent(BROWSER_UA)
            .timeout(Duration::from_secs(ARTICLE_TIMEOUT_SECS))
            .build()?;
        Ok(Self { client })
    }

    /// 抽取 `canonical_url` 对应的正文。`first_news_id` 用于审计。
    pub async fn extract(&self, canonical_url: &str, first_news_id: Option<&str>) -> ArticleExtractOutput {
        let now = Utc::now();
        match self.do_extract(canonical_url, first_news_id, now).await {
            Ok(out) => out,
            Err(e) => failure(canonical_url, first_news_id, now, e.reason, &e.message),
        }
    }

    /// 按 source 策略抽取正文（spec references/news/article-strategies.md）。
    /// 返回 `None` 表示该源 `TitleIsContent`（快讯，标题即全文，无需抓取、不存正文）。
    /// 其余策略返回 `ArticleExtractOutput`（成功有 content；失败 error 填充供调用方记 log）。
    pub async fn extract_for_source(
        &self,
        source: &str,
        canonical_url: &str,
        first_news_id: Option<&str>,
    ) -> Option<ArticleExtractOutput> {
        let now = Utc::now();
        let strategy = strategy_for(source);
        if strategy == ArticleStrategy::TitleIsContent {
            return None;
        }
        let res = match strategy {
            ArticleStrategy::ClsNextData => self.extract_cls(canonical_url).await,
            ArticleStrategy::WallstreetcnApi => self.extract_wallstreetcn(canonical_url).await,
            ArticleStrategy::Kr36Meta => self.extract_36kr(canonical_url).await,
            ArticleStrategy::StaticSelector { selector, strip } => {
                self.extract_static(canonical_url, selector, strip).await
            }
            // 联合早报正文末尾恒有 .warning 反广告拦截提示（"内容可能不完整…"），剔除。
            ArticleStrategy::ZaochenbaoStatic => {
                self.extract_static(canonical_url, "#article-body", &[".warning"]).await
            }
            ArticleStrategy::CankaoInlineScript => self.extract_cankao(canonical_url).await,
            ArticleStrategy::TitleIsContent => unreachable!(),
            ArticleStrategy::Generic => {
                return Some(self.extract(canonical_url, first_news_id).await)
            }
        };
        Some(match res {
            Ok((title, content)) => {
                if content.chars().count() < ARTICLE_MIN_CONTENT_CHARS {
                    failure(canonical_url, first_news_id, now, ArticleExtractReason::TooShort, "content too short")
                } else {
                    ArticleExtractOutput {
                        article: ArticleContent {
                            url: canonical_url.to_string(),
                            first_news_id: first_news_id.map(|s| s.to_string()),
                            title,
                            content: Some(content),
                            payload: serde_json::json!({"provider": "article_extractor", "strategy": format!("{strategy:?}")}),
                            fetched_at: now,
                            warning: None,
                        },
                        error: None,
                    }
                }
            }
            Err(e) => failure(canonical_url, first_news_id, now, e.reason, &e.message),
        })
    }

    /// GET 并按字符集 decode 成 HTML 文本。
    async fn fetch_decoded(&self, url: &str) -> Result<String, ExtractErr> {
        let resp = self.client.get(url).send().await.map_err(|e| ExtractErr {
            reason: if e.is_timeout() { ArticleExtractReason::Timeout } else { ArticleExtractReason::Network },
            message: e.to_string(),
        })?;
        if !resp.status().is_success() {
            return Err(ExtractErr {
                reason: ArticleExtractReason::HttpStatus,
                message: format!("http status {}", resp.status()),
            });
        }
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        let bytes = resp.bytes().await.map_err(|e| ExtractErr {
            reason: ArticleExtractReason::Network,
            message: e.to_string(),
        })?;
        Ok(decode_html(&bytes, &ct))
    }

    /// 财联社：detail 页 `__NEXT_DATA__` JSON → articleDetail.{title,content}。
    async fn extract_cls(&self, url: &str) -> Result<(Option<String>, String), ExtractErr> {
        let id = last_path_segment(url).ok_or_else(|| parse_err("cls: no id in url"))?;
        let page = self
            .fetch_decoded(&format!("https://www.cls.cn/detail/{id}"))
            .await?;
        let json = extract_script_json(&page, "__NEXT_DATA__")
            .ok_or_else(|| parse_err("cls: no __NEXT_DATA__"))?;
        let detail = json
            .pointer("/props/pageProps/articleDetail")
            .ok_or_else(|| parse_err("cls: no articleDetail"))?;
        let content_html = detail.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let title = detail.get("title").and_then(|v| v.as_str()).map(|s| s.to_string());
        let text = html_to_text(content_html);
        if text.is_empty() {
            return Err(parse_err("cls: empty content"));
        }
        Ok((title, text))
    }

    /// 华尔街见闻：api-one JSON。按 url 判断 lives（快讯，content_text）还是
    /// articles（长文，content HTML）—— 主站 `wallstreetcn` 渠道会混下发两类。
    async fn extract_wallstreetcn(&self, url: &str) -> Result<(Option<String>, String), ExtractErr> {
        let id = last_path_segment(url).ok_or_else(|| parse_err("wscn: no id"))?;
        let is_article = url.contains("/articles/");
        let api = if is_article {
            format!("https://api-one.wallstcn.com/apiv1/content/articles/{id}?extract=0")
        } else {
            format!("https://api-one.wallstcn.com/apiv1/content/lives/{id}")
        };
        let resp = self.client.get(&api).send().await.map_err(|e| ExtractErr {
            reason: if e.is_timeout() { ArticleExtractReason::Timeout } else { ArticleExtractReason::Network },
            message: e.to_string(),
        })?;
        let json: serde_json::Value = resp.json().await.map_err(|e| ExtractErr {
            reason: ArticleExtractReason::ParseError,
            message: e.to_string(),
        })?;
        let data = json.get("data").ok_or_else(|| parse_err("wscn: no data"))?;
        // articles 仅有 content(HTML)；lives 首选 content_text(纯文本)，回落 content。
        let text = data
            .get("content_text")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| data.get("content").and_then(|v| v.as_str()).map(html_to_text))
            .unwrap_or_default();
        let title = data.get("title").and_then(|v| v.as_str()).map(|s| s.to_string());
        if text.is_empty() {
            return Err(parse_err("wscn: empty content"));
        }
        Ok((title, text))
    }

    /// 参考消息：正文在内联 JS 变量 `var contentTxt = "…";`（DOM 容器是空壳由 JS 填充）。
    async fn extract_cankao(&self, url: &str) -> Result<(Option<String>, String), ExtractErr> {
        let page = self.fetch_decoded(url).await?;
        let raw = extract_js_string_var(&page, "contentTxt")
            .ok_or_else(|| parse_err("cankao: no contentTxt var"))?;
        // JS 字符串字面量内的转义：\/ → /，\" → "，\n 等先还原再 strip tags。
        let unescaped = raw
            .replace("\\/", "/")
            .replace("\\\"", "\"")
            .replace("\\n", "\n")
            .replace("\\r", "")
            .replace("\\t", "\t");
        let text = html_to_text(&unescaped);
        if text.is_empty() {
            return Err(parse_err("cankao: empty content"));
        }
        Ok((None, text))
    }

    /// 36 氪：快讯页 `<meta name=description>` == 正文。
    async fn extract_36kr(&self, url: &str) -> Result<(Option<String>, String), ExtractErr> {
        let id = last_path_segment(url).ok_or_else(|| parse_err("36kr: no id"))?;
        let page = self
            .fetch_decoded(&format!("https://www.36kr.com/newsflashes/{id}"))
            .await?;
        let doc = Html::parse_document(&page);
        let sel = Selector::parse(r#"meta[name="description"]"#).unwrap();
        let text = doc
            .select(&sel)
            .next()
            .and_then(|el| el.value().attr("content"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if text.is_empty() {
            return Err(parse_err("36kr: no description"));
        }
        Ok((None, text))
    }

    /// 静态页：按 selector 抽正文文本，剔除 `strip` selector 命中的 noisy 子树
    /// （免责声明 / 反广告拦截提示等），它们随正文容器一起被 DOM 文本扫描收进来。
    async fn extract_static(
        &self,
        url: &str,
        selector: &str,
        strip: &[&str],
    ) -> Result<(Option<String>, String), ExtractErr> {
        let page = self.fetch_decoded(url).await?;
        let doc = Html::parse_document(&page);
        let sel = Selector::parse(selector)
            .map_err(|_| parse_err("static: bad selector"))?;
        let el = doc.select(&sel).next().ok_or_else(|| parse_err("static: selector miss"))?;
        // 收集要跳过的子树 NodeId（strip selector 命中的节点及其后代）。
        let mut skip: std::collections::HashSet<ego_tree::NodeId> = std::collections::HashSet::new();
        for s in strip {
            if let Ok(strip_sel) = Selector::parse(s) {
                for node in el.select(&strip_sel) {
                    skip.insert(node.id());
                }
            }
        }
        let mut buf = String::new();
        collect_text(el, &mut buf, &skip);
        let text = clean_whitespace(&buf);
        if text.is_empty() {
            return Err(parse_err("static: empty"));
        }
        Ok((None, text))
    }

    async fn do_extract(
        &self,
        canonical_url: &str,
        first_news_id: Option<&str>,
        now: OccurredAt,
    ) -> Result<ArticleExtractOutput, ExtractErr> {
        let resp = self
            .client
            .get(canonical_url)
            .send()
            .await
            .map_err(|e| ExtractErr {
                reason: if e.is_timeout() {
                    ArticleExtractReason::Timeout
                } else {
                    ArticleExtractReason::Network
                },
                message: e.to_string(),
            })?;

        if !resp.status().is_success() {
            return Err(ExtractErr {
                reason: ArticleExtractReason::HttpStatus,
                message: format!("http status {}", resp.status()),
            });
        }

        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        let body_bytes = resp
            .bytes()
            .await
            .map_err(|e| ExtractErr {
                reason: ArticleExtractReason::Network,
                message: e.to_string(),
            })?;
        if body_bytes.len() > ARTICLE_MAX_BODY_BYTES {
            return Err(ExtractErr {
                reason: ArticleExtractReason::HttpStatus,
                message: format!("body exceeds {} bytes", ARTICLE_MAX_BODY_BYTES),
            });
        }

        // Content-Type 不是 HTML-like：返回失败 + 细分原因 unsupported_content_type
        if !ct.is_empty() && !is_html_like(&ct) {
            return Err(ExtractErr {
                reason: ArticleExtractReason::UnsupportedContentType,
                message: format!("unsupported content-type: {}", ct),
            });
        }

        // 字符集解码：很多中文站（如早晨报 zaochenbao）是 GBK/GB18030，
        // 直接 from_utf8_lossy 会整篇乱码。先从 Content-Type charset 取，
        // 取不到再嗅探 <meta charset=...>，最后用 encoding_rs 解码。
        let html = decode_html(&body_bytes, &ct);
        let (title, content) = extract_main(&html);

        let too_short = content
            .as_deref()
            .map(|c| c.chars().count() < ARTICLE_MIN_CONTENT_CHARS)
            .unwrap_or(true);

        if too_short {
            // 失败缓存 + 细分原因 too_short（spec §5 article stage failure 必填 reason）
            let article = ArticleContent {
                url: canonical_url.to_string(),
                first_news_id: first_news_id.map(|s| s.to_string()),
                title,
                content: None,
                payload: serde_json::json!({
                    "provider": "article_extractor",
                    "reason": ArticleExtractReason::TooShort.as_str(),
                }),
                fetched_at: now,
                warning: Some(WarningCode::ArticleMissing),
            };
            return Ok(ArticleExtractOutput {
                article,
                error: Some(ArticleExtractFailure {
                    code: ErrorCode::ArticleExtractFailed,
                    reason: ArticleExtractReason::TooShort,
                    message: "content empty or too short".to_string(),
                }),
            });
        }

        Ok(ArticleExtractOutput {
            article: ArticleContent {
                url: canonical_url.to_string(),
                first_news_id: first_news_id.map(|s| s.to_string()),
                title,
                content,
                payload: serde_json::json!({"provider": "article_extractor"}),
                fetched_at: now,
                warning: None,
            },
            error: None,
        })
    }
}

struct ExtractErr {
    reason: ArticleExtractReason,
    message: String,
}

fn parse_err(msg: &str) -> ExtractErr {
    ExtractErr { reason: ArticleExtractReason::ParseError, message: msg.to_string() }
}

/// 取 URL 最后一个路径段（去 query/fragment），用于从 detail url 提 id。
fn last_path_segment(url: &str) -> Option<String> {
    let no_q = url.split(['?', '#']).next().unwrap_or(url);
    no_q.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// 从 HTML 里取 `<script id="<id>" ...>...</script>` 内的 JSON。
fn extract_script_json(html: &str, id: &str) -> Option<serde_json::Value> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse(&format!(r#"script#{id}"#)).ok()?;
    let raw = doc.select(&sel).next()?.text().collect::<String>();
    serde_json::from_str(raw.trim()).ok()
}

/// 从页面内联脚本里取 `<name> = "…";` 的字符串字面量（含转义，未还原）。
/// `=` 两侧空白可有可无（实测参考消息写法是 `contentTxt ="…`）。
/// 同名变量可能多次出现（赋值 / 引用），逐个尝试直到命中 `= "`。
/// 用于正文藏在 JS 变量里、DOM 容器为空壳的源（如参考消息）。
fn extract_js_string_var(html: &str, name: &str) -> Option<String> {
    let bytes = html.as_bytes();
    let mut from = 0;
    while let Some(rel) = html[from..].find(name) {
        let after_name = from + rel + name.len();
        // 跳过 name 后的空白，要求紧跟 `=`，再跳过空白，要求紧跟 `"`。
        let mut k = after_name;
        while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
            k += 1;
        }
        if k < bytes.len() && bytes[k] == b'=' {
            k += 1;
            while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
                k += 1;
            }
            if k < bytes.len() && bytes[k] == b'"' {
                let str_start = k + 1;
                // 找到未被 `\` 转义的收尾 `"`。
                let mut i = str_start;
                while i < bytes.len() {
                    if bytes[i] == b'"' {
                        let mut bs = 0;
                        let mut j = i;
                        while j > 0 && bytes[j - 1] == b'\\' {
                            bs += 1;
                            j -= 1;
                        }
                        if bs % 2 == 0 {
                            return Some(html[str_start..i].to_string());
                        }
                    }
                    i += 1;
                }
            }
        }
        from = after_name;
    }
    None
}

/// HTML 片段 → 纯文本（strip tags + 规整空白）。
fn html_to_text(html: &str) -> String {
    let frag = Html::parse_fragment(html);
    let mut buf = String::new();
    let no_skip = std::collections::HashSet::new();
    for node in frag.tree.root().children() {
        walk_node(node, &mut buf, &no_skip);
    }
    clean_whitespace(&buf)
}

fn failure(
    canonical_url: &str,
    first_news_id: Option<&str>,
    now: OccurredAt,
    reason: ArticleExtractReason,
    message: &str,
) -> ArticleExtractOutput {
    // 即使失败也保存失败缓存（spec：避免短时间反复抓取）。
    let article = ArticleContent {
        url: canonical_url.to_string(),
        first_news_id: first_news_id.map(|s| s.to_string()),
        title: None,
        content: None,
        payload: serde_json::json!({
            "provider": "article_extractor",
            "reason": reason.as_str(),
            "error": message,
        }),
        fetched_at: now,
        warning: Some(WarningCode::ArticleMissing),
    };
    ArticleExtractOutput {
        article,
        error: Some(ArticleExtractFailure {
            code: ErrorCode::ArticleExtractFailed,
            reason,
            message: message.to_string(),
        }),
    }
}

fn is_html_like(content_type: &str) -> bool {
    content_type.contains("html")
        || content_type.contains("xhtml")
        || content_type.contains("text/plain")
}

/// 按字符集把 HTML 字节解码成 String。
/// 顺序：Content-Type charset → `<meta charset>` / `<meta http-equiv>` 嗅探 → UTF-8 兜底。
/// 用 encoding_rs，支持 gbk / gb2312 / gb18030 / big5 / utf-8 等。
fn decode_html(bytes: &[u8], content_type: &str) -> String {
    let label = charset_from_content_type(content_type)
        .or_else(|| sniff_meta_charset(bytes))
        .unwrap_or_else(|| "utf-8".to_string());
    let enc = encoding_rs::Encoding::for_label(label.as_bytes())
        .unwrap_or(encoding_rs::UTF_8);
    let (cow, _, _) = enc.decode(bytes);
    cow.into_owned()
}

fn charset_from_content_type(ct: &str) -> Option<String> {
    // e.g. "text/html; charset=gbk"
    let idx = ct.find("charset=")?;
    let raw = ct[idx + "charset=".len()..].trim();
    let val = raw
        .trim_matches(|c| c == '"' || c == '\'')
        .split(|c| c == ';' || c == ' ')
        .next()?
        .trim();
    if val.is_empty() {
        None
    } else {
        Some(val.to_string())
    }
}

/// 在 HTML 头部前 2KB 内 ascii 嗅探 `<meta charset="...">` 或
/// `<meta http-equiv="Content-Type" content="...; charset=...">`。
fn sniff_meta_charset(bytes: &[u8]) -> Option<String> {
    let head_len = bytes.len().min(2048);
    // 用 lossy 只为嗅探 ascii 标记，不影响最终解码。
    let head = String::from_utf8_lossy(&bytes[..head_len]).to_ascii_lowercase();
    let idx = head.find("charset=")?;
    let rest = &head[idx + "charset=".len()..];
    let val: String = rest
        .trim_start_matches(|c| c == '"' || c == '\'' || c == ' ')
        .chars()
        .take_while(|c| {
            c.is_ascii_alphanumeric() || *c == '-' || *c == '_'
        })
        .collect();
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

/// 提取标题 + 主正文。简单策略：
/// - title: `<meta property="og:title">` → `<title>`
/// - content: `<article>` → `<main>` → `<body>` 文本，剔除 script/style/nav 等。
pub fn extract_main(html: &str) -> (Option<String>, Option<String>) {
    let doc = Html::parse_document(html);
    let title = pick_title(&doc);
    let body_text = pick_main_text(&doc);
    let cleaned = body_text.map(|t| clean_whitespace(&t));
    (title, cleaned)
}

fn pick_title(doc: &Html) -> Option<String> {
    let og_selector = Selector::parse("meta[property='og:title']").ok()?;
    if let Some(el) = doc.select(&og_selector).next() {
        if let Some(c) = el.value().attr("content") {
            let t = c.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    let title_selector = Selector::parse("title").ok()?;
    if let Some(el) = doc.select(&title_selector).next() {
        let t = el.text().collect::<String>();
        let t = t.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    None
}

fn pick_main_text(doc: &Html) -> Option<String> {
    for sel_str in &["article", "main", "div.article", "div#article", "body"] {
        let sel = match Selector::parse(sel_str) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if let Some(el) = doc.select(&sel).next() {
            let mut buf = String::new();
            collect_text(el, &mut buf, &std::collections::HashSet::new());
            let trimmed = buf.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn collect_text(
    el: scraper::ElementRef<'_>,
    buf: &mut String,
    skip: &std::collections::HashSet<ego_tree::NodeId>,
) {
    // 递归收集 text nodes，跳过 noisy 子树（tag 黑名单 + skip NodeId 集）。
    walk_node(*el, buf, skip);
}

fn walk_node(
    node: ego_tree::NodeRef<'_, scraper::Node>,
    buf: &mut String,
    skip: &std::collections::HashSet<ego_tree::NodeId>,
) {
    use scraper::Node;
    for child in node.children() {
        if skip.contains(&child.id()) {
            continue; // strip selector 命中的子树整体跳过
        }
        match child.value() {
            Node::Element(elem) => {
                let name = elem.name();
                if matches!(
                    name,
                    "script"
                        | "style"
                        | "noscript"
                        | "iframe"
                        | "nav"
                        | "header"
                        | "footer"
                        | "aside"
                ) {
                    continue;
                }
                walk_node(child, buf, skip);
            }
            Node::Text(t) => {
                buf.push_str(t);
                buf.push(' ');
            }
            _ => {}
        }
    }
}

fn clean_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_ws && !out.is_empty() {
                out.push(' ');
            }
            last_ws = true;
        } else {
            out.push(ch);
            last_ws = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charset_from_content_type_parses() {
        assert_eq!(
            charset_from_content_type("text/html; charset=gbk").as_deref(),
            Some("gbk")
        );
        assert_eq!(
            charset_from_content_type("text/html;charset=UTF-8").as_deref(),
            Some("UTF-8")
        );
        assert_eq!(charset_from_content_type("text/html").as_deref(), None);
    }

    #[test]
    fn sniff_meta_charset_finds_gbk() {
        let html = br#"<!DOCTYPE html><html><head><meta charset="gbk"><title>x</title>"#;
        assert_eq!(sniff_meta_charset(html).as_deref(), Some("gbk"));
    }

    #[test]
    fn decode_html_gbk_roundtrip() {
        // "新闻" GBK 编码字节
        let (gbk_bytes, _, _) = encoding_rs::GBK.encode("新闻正文");
        let mut body = b"<html><head><meta charset=\"gbk\"></head><body>".to_vec();
        body.extend_from_slice(&gbk_bytes);
        body.extend_from_slice(b"</body></html>");
        let html = decode_html(&body, "text/html");
        assert!(html.contains("新闻正文"));
    }

    #[test]
    fn extract_main_basic() {
        let html = r#"<html><head><title>T</title></head>
            <body><article>This is the article body. It is long enough to pass min length.
              And more text here for good measure: 中文内容也算字符长度的一部分用于计算正文长度。</article></body></html>"#;
        let (title, content) = extract_main(html);
        assert_eq!(title.as_deref(), Some("T"));
        assert!(content.as_deref().unwrap().contains("article body"));
    }

    /// Spec §5: article-stage failure 必须 code = article_extract_failed，
    /// reason 字段是分类字符串。
    #[test]
    fn failure_produces_article_extract_failed_with_reason() {
        let now = Utc::now();
        for r in [
            ArticleExtractReason::Network,
            ArticleExtractReason::Timeout,
            ArticleExtractReason::TooShort,
            ArticleExtractReason::UnsupportedContentType,
            ArticleExtractReason::HttpStatus,
            ArticleExtractReason::ParseError,
        ] {
            let out = failure("https://a.com/x", None, now, r, "boom");
            let err = out.error.expect("should have error");
            assert_eq!(err.code, ErrorCode::ArticleExtractFailed);
            assert_eq!(err.reason, r);
            // article cache 写入
            assert_eq!(out.article.url, "https://a.com/x");
            assert!(out.article.content.is_none());
            assert_eq!(out.article.warning, Some(WarningCode::ArticleMissing));
            // payload.reason 是字符串
            let payload_reason = out.article.payload.get("reason").and_then(|v| v.as_str());
            assert_eq!(payload_reason, Some(r.as_str()));
        }
    }

    #[test]
    fn reason_as_str_covers_all_variants() {
        assert_eq!(ArticleExtractReason::Network.as_str(), "network");
        assert_eq!(ArticleExtractReason::Timeout.as_str(), "timeout");
        assert_eq!(ArticleExtractReason::TooShort.as_str(), "too_short");
        assert_eq!(
            ArticleExtractReason::UnsupportedContentType.as_str(),
            "unsupported_content_type"
        );
        assert_eq!(ArticleExtractReason::HttpStatus.as_str(), "http_status");
        assert_eq!(ArticleExtractReason::ParseError.as_str(), "parse_error");
    }

    #[test]
    fn extract_js_string_var_stops_at_unescaped_quote() {
        // 收尾引号前是转义引号 \" 时不应提前结束。
        let html = r#"<script>var contentTxt = "<p>he said \"hi\"<\/p>"; var x = 1;</script>"#;
        let raw = extract_js_string_var(html, "contentTxt").expect("should find var");
        assert_eq!(raw, r#"<p>he said \"hi\"<\/p>"#);
    }

    #[test]
    fn strategy_for_maps_new_sources() {
        assert!(matches!(
            strategy_for("newsnow:wallstreetcn"),
            ArticleStrategy::WallstreetcnApi
        ));
        assert!(matches!(
            strategy_for("newsnow:fastbull-news"),
            ArticleStrategy::StaticSelector { selector: ".news-detail-content", .. }
        ));
        assert!(matches!(
            strategy_for("newsnow:cankaoxiaoxi"),
            ArticleStrategy::CankaoInlineScript
        ));
        assert!(matches!(
            strategy_for("newsnow:sputniknewscn"),
            ArticleStrategy::StaticSelector { selector: ".article__body", .. }
        ));
    }

    /// 实网烟测：对 4 个新登记渠道各取 NewsNow 首条 url 跑抽取，打印是否拿到正文。
    /// 默认 #[ignore]（依赖外网 + 实时数据）。运行：
    ///   cargo test --manifest-path src-tauri/Cargo.toml \
    ///     infrastructure::news::article_extractor::tests::live_smoke_new_sources -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_smoke_new_sources() {
        let ex = ArticleExtractor::new().unwrap();
        let client = reqwest::Client::builder()
            .user_agent(BROWSER_UA)
            .build()
            .unwrap();
        // 每个源映射其正文末尾**不应**出现的 boilerplate noise（剥离验证）。
        let noise: &[(&str, &str)] = &[
            ("fastbull-news", "责任自负"),
            ("zaobao", "内容可能不完整"),
        ];
        for ch in ["wallstreetcn", "fastbull-news", "cankaoxiaoxi", "sputniknewscn", "zaobao"] {
            let feed = format!("https://newsnow.busiyi.world/api/s?id={ch}&latest");
            let json: serde_json::Value = client
                .get(&feed)
                .header("Origin", "https://newsnow.busiyi.world")
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let url = json
                .pointer("/items/0/url")
                .and_then(|v| v.as_str())
                .expect("first item url")
                .to_string();
            let out = ex
                .extract_for_source(&format!("newsnow:{ch}"), &url, None)
                .await;
            match out {
                Some(o) => {
                    let len = o.article.content.as_ref().map(|c| c.chars().count()).unwrap_or(0);
                    let preview: String = o
                        .article
                        .content
                        .as_deref()
                        .unwrap_or("")
                        .chars()
                        .take(60)
                        .collect();
                    let content = o.article.content.as_deref().unwrap_or("");
                    let tail: String = content.chars().rev().take(50).collect::<Vec<_>>().into_iter().rev().collect();
                    println!(
                        "[{ch}] url={url} ok={} len={len} err={:?}\n    head: {preview}\n    tail: …{tail}",
                        o.error.is_none(),
                        o.error.as_ref().map(|e| &e.reason)
                    );
                    assert!(o.error.is_none(), "{ch}: extraction failed: {:?}", o.error);
                    if let Some((_, n)) = noise.iter().find(|(c, _)| *c == ch) {
                        assert!(!content.contains(n), "{ch}: content still contains noise {n:?}");
                    }
                }
                None => println!("[{ch}] TitleIsContent (skipped)"),
            }
        }
    }
}
