//! 联网搜索 `web_search` —— 可插拔多源 + 并行聚合（Infra 默认工具，config-gated）。
//!
//! Spec: docs/design/agent-infra-module.md §3.6 `web_search`
//!
//! 参考 hermes-agent 的 `WebSearchProvider` 抽象（`/Users/tu/zhigang/code_agent/hermes-agent`）。
//! 差异：hermes 是**单源 dispatch**；本项目按用户要求做**并行 fan-out + 去重聚合**（多源同时搜、合并）。
//!
//! - `WebSearchProvider` trait：每个搜索源一个实现（providers.rs）。
//! - `MultiWebSearch`：对所有 enabled 源并行 search → 单源失败容错 → 按 canonical URL 去重
//!   → 跨源 round-robin 交错 → 回带 `source` 标签的聚合列表。
//! - 前端不直接发外部 HTTP，全部走 Rust（架构红线）。provider wire format 是 infra 细节。

mod providers;

#[cfg(test)]
mod tests;

use crate::domain::agent::{SideEffect, ToolSpec};
use crate::domain::shared::ErrorCode;
use crate::infrastructure::agent::tool_registry::{
    FnToolHandler, RegisterError, ToolHandler, ToolHandlerFuture, ToolHandlerOutput, ToolInvocation,
    ToolRegistry,
};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

const WEB_SEARCH_TIMEOUT_MS: u64 = 20_000;
const DEFAULT_MAX_RESULTS: usize = 8;
const AGG_CAP: usize = 20;
/// 共享 User-Agent（DuckDuckGo HTML 抓取等需要一个像浏览器的 UA）。
const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

/// 一条搜索结果（聚合后回给 agent）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    /// 来自哪个 provider（duckduckgo / jina / bocha / tavily）。
    pub source: String,
}

#[derive(Debug)]
pub enum WebSearchError {
    Http(String),
    Parse(String),
    Auth(String),
}

impl std::fmt::Display for WebSearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebSearchError::Http(m) => write!(f, "http: {m}"),
            WebSearchError::Parse(m) => write!(f, "parse: {m}"),
            WebSearchError::Auth(m) => write!(f, "auth: {m}"),
        }
    }
}

/// 一个搜索源。每个实现负责把自家响应归一成 `WebSearchResult`。
#[async_trait::async_trait]
pub trait WebSearchProvider: Send + Sync {
    fn name(&self) -> &str;
    async fn search(&self, query: &str, max_results: usize)
        -> Result<Vec<WebSearchResult>, WebSearchError>;
}

/// 多源并行聚合器。
pub struct MultiWebSearch {
    providers: Vec<Box<dyn WebSearchProvider>>,
}

impl MultiWebSearch {
    pub fn new(providers: Vec<Box<dyn WebSearchProvider>>) -> Self {
        Self { providers }
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    pub fn provider_names(&self) -> Vec<String> {
        self.providers.iter().map(|p| p.name().to_string()).collect()
    }

    /// 对所有源并行搜索 → 容错 → round-robin 交错 + 按 URL 去重 → 截断。
    pub async fn search(&self, query: &str, max_results: usize) -> Vec<WebSearchResult> {
        let futs = self.providers.iter().map(|p| {
            let name = p.name().to_string();
            async move { (name, p.search(query, max_results).await) }
        });
        let per = futures_util::future::join_all(futs).await;

        let mut lists: Vec<Vec<WebSearchResult>> = Vec::new();
        for (name, res) in per {
            match res {
                Ok(rs) => lists.push(rs),
                Err(e) => tracing::warn!(
                    target: "agent.web_search", provider = %name, error = %e,
                    "provider search failed (ignored, others continue)"
                ),
            }
        }
        interleave_dedupe(lists, max_results)
    }
}

/// canonical URL key（去重用）：trim、去尾斜杠、小写。
fn canonical_url(u: &str) -> String {
    u.trim().trim_end_matches('/').to_ascii_lowercase()
}

/// 跨源 round-robin 交错（保证每源头部结果都露脸），按 canonical URL 去重，截断到 `max*2`（≤ AGG_CAP）。
fn interleave_dedupe(lists: Vec<Vec<WebSearchResult>>, max: usize) -> Vec<WebSearchResult> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let widest = lists.iter().map(|l| l.len()).max().unwrap_or(0);
    let cap = max.max(1).saturating_mul(2).min(AGG_CAP);
    'outer: for i in 0..widest {
        for l in &lists {
            if let Some(r) = l.get(i) {
                let key = canonical_url(&r.url);
                if !key.is_empty() && seen.insert(key) {
                    out.push(r.clone());
                    if out.len() >= cap {
                        break 'outer;
                    }
                }
            }
        }
    }
    out
}

/// provider key / 开关配置（adapter 从设置注入；构造时按启用项 build 出聚合器）。
///
/// 现状（2026-06 实测）：**无 key 的免费源基本失效** —— DuckDuckGo html/lite 被反爬挡（HTTP 202
/// challenge，可能与发起 IP 有关，住宅 IP 或许仍可）、Jina/SearXNG 已要 key 或封 bot。故除 DuckDuckGo
/// （IP 相关、best-effort、聚合容错）外，其余源都需各自的（免费额度）key。
#[derive(Debug, Clone, Default)]
pub struct WebSearchConfig {
    /// DuckDuckGo 无 key 抓取；可能被目标站反爬挡（取决于发起 IP）。聚合层容错（失败忽略）。
    pub enable_duckduckgo: bool,
    /// Jina 现已强制需要 key（s.jina.ai 401 AuthenticationRequired）。
    pub jina_key: Option<String>,
    pub bocha_key: Option<String>,
    pub tavily_key: Option<String>,
}

impl WebSearchConfig {
    /// 按启用项构造聚合器（共享一个 reqwest client）。
    pub fn build(&self, client: reqwest::Client) -> MultiWebSearch {
        let mut ps: Vec<Box<dyn WebSearchProvider>> = Vec::new();
        if self.enable_duckduckgo {
            ps.push(Box::new(providers::DuckDuckGo::new(client.clone())));
        }
        if let Some(k) = self.jina_key.clone().filter(|k| !k.is_empty()) {
            ps.push(Box::new(providers::Jina::new(client.clone(), k)));
        }
        if let Some(k) = self.bocha_key.clone().filter(|k| !k.is_empty()) {
            ps.push(Box::new(providers::Bocha::new(client.clone(), k)));
        }
        if let Some(k) = self.tavily_key.clone().filter(|k| !k.is_empty()) {
            ps.push(Box::new(providers::Tavily::new(client.clone(), k)));
        }
        MultiWebSearch::new(ps)
    }
}

/// 注册 `web_search` 工具（Infra 默认工具，bootstrap 注入已配好的聚合器）。
pub fn register_web_search_tool(
    registry: &ToolRegistry,
    search: Arc<MultiWebSearch>,
) -> Result<(), RegisterError> {
    let spec = ToolSpec::new(
        "web_search",
        "联网搜索（多源并行聚合）。query 查实时网页 / 资料，maxResults 控制每源条数（默认 8）。\
         返回 [{title,url,snippet,source}]，source 标明来自哪个搜索源；本地资讯库用 fetch_news，\
         开放互联网才用本工具。只读。",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "搜索关键词 / 自然语言问题" },
                "maxResults": { "type": "integer", "description": "每源返回上限，默认 8（聚合去重后更少）" }
            },
            "required": ["query"]
        }),
        vec![r#"<use_tool name="web_search">{"query":"贵州茅台 2024 三季报 业绩"}</use_tool>"#.into()],
        WEB_SEARCH_TIMEOUT_MS,
        SideEffect::None,
    );

    let handler: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(move |inv: ToolInvocation| {
        let search = search.clone();
        Box::pin(async move {
            let query = inv
                .input
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if query.is_empty() {
                return ToolHandlerOutput::err(
                    json!({ "reason": "invalid_input", "message": "query 不能为空" }),
                    ErrorCode::InvalidInput,
                );
            }
            if search.is_empty() {
                return ToolHandlerOutput::err(
                    json!({
                        "reason": "invalid_input",
                        "message": "未配置任何搜索源：请在设置启用 DuckDuckGo/Jina 或填 Bocha/Tavily key"
                    }),
                    ErrorCode::InvalidInput,
                );
            }
            let max = inv
                .input
                .get("maxResults")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_MAX_RESULTS)
                .clamp(1, AGG_CAP);
            let results = search.search(&query, max).await;
            ToolHandlerOutput::ok(json!({
                "results": results,
                "providers": search.provider_names(),
            }))
        }) as ToolHandlerFuture
    }));

    registry.register_tool(spec, handler)
}
