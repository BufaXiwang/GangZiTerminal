//! web_search 测试：聚合器（去重 / 交错 / 并行容错）hermetic + DuckDuckGo live。

use super::*;
use crate::infrastructure::agent::tool_registry::ToolRegistry;

fn r(title: &str, url: &str, source: &str) -> WebSearchResult {
    WebSearchResult {
        title: title.into(),
        url: url.into(),
        snippet: "".into(),
        source: source.into(),
    }
}

struct Mock {
    name: String,
    out: Vec<WebSearchResult>,
    fail: bool,
}

#[async_trait::async_trait]
impl WebSearchProvider for Mock {
    fn name(&self) -> &str {
        &self.name
    }
    async fn search(&self, _q: &str, _m: usize) -> Result<Vec<WebSearchResult>, WebSearchError> {
        if self.fail {
            Err(WebSearchError::Http("boom".into()))
        } else {
            Ok(self.out.clone())
        }
    }
}

fn mock(name: &str, out: Vec<WebSearchResult>) -> Box<dyn WebSearchProvider> {
    Box::new(Mock { name: name.into(), out, fail: false })
}
fn mock_fail(name: &str) -> Box<dyn WebSearchProvider> {
    Box::new(Mock { name: name.into(), out: vec![], fail: true })
}

#[test]
fn interleave_dedupe_mixes_sources_and_drops_dup_urls() {
    let a = vec![r("A1", "https://x.com/1", "a"), r("A2", "https://x.com/2", "a")];
    // b 的第一条与 a 第二条同 URL（去尾斜杠后相同）→ 去重。
    let b = vec![r("B1", "https://x.com/2/", "b"), r("B2", "https://y.com/9", "b")];
    let agg = interleave_dedupe(vec![a, b], 8);
    let urls: Vec<&str> = agg.iter().map(|x| x.url.as_str()).collect();
    // round-robin：a[0], b[0](dedup掉，与a后续重?不), ... 关键断言：URL 唯一 + 跨源都在。
    assert_eq!(agg.len(), 3, "x.com/2 去重后应剩 3 条；got {:?}", urls);
    let canon: std::collections::HashSet<String> =
        agg.iter().map(|x| canonical_url(&x.url)).collect();
    assert_eq!(canon.len(), 3, "URL 必须唯一");
    assert!(agg.iter().any(|x| x.source == "a") && agg.iter().any(|x| x.source == "b"));
}

#[tokio::test]
async fn aggregator_runs_parallel_and_tolerates_provider_failure() {
    let agg = MultiWebSearch::new(vec![
        mock("a", vec![r("A1", "https://a.com/1", "a")]),
        mock_fail("b"), // 这个失败应被忽略，不拖垮整体
        mock("c", vec![r("C1", "https://c.com/1", "c")]),
    ]);
    let out = agg.search("q", 8).await;
    assert_eq!(out.len(), 2, "失败源忽略，其余两源结果都在");
    assert!(out.iter().any(|x| x.source == "a"));
    assert!(out.iter().any(|x| x.source == "c"));
}

#[test]
fn config_build_counts_enabled_providers() {
    let client = reqwest::Client::new();
    // 免费双源：sogou 排前（中文主力），duckduckgo 其次。
    let both = WebSearchConfig { enable_duckduckgo: true, enable_sogou: true }.build(client.clone());
    assert_eq!(both.provider_names(), vec!["sogou".to_string(), "duckduckgo".to_string()]);
    let ddg_only = WebSearchConfig { enable_duckduckgo: true, enable_sogou: false }.build(client.clone());
    assert_eq!(ddg_only.provider_names(), vec!["duckduckgo".to_string()]);
    let off = WebSearchConfig { enable_duckduckgo: false, enable_sogou: false }.build(client);
    assert!(off.is_empty());
}

#[tokio::test]
async fn tool_empty_config_returns_invalid_input() {
    let registry = ToolRegistry::new_without_persist();
    register_web_search_tool(&registry, Arc::new(MultiWebSearch::new(vec![]))).unwrap();
    let res = registry
        .dispatch_tool_call("run", "tc_1".into(), "web_search", serde_json::json!({"query":"x"}))
        .await
        .unwrap();
    assert!(res.is_error);
    assert_eq!(res.error_code, Some(ErrorCode::InvalidInput));
}

#[tokio::test]
async fn tool_returns_aggregated_results() {
    let registry = ToolRegistry::new_without_persist();
    let agg = MultiWebSearch::new(vec![mock(
        "a",
        vec![r("茅台业绩", "https://a.com/maotai", "a")],
    )]);
    register_web_search_tool(&registry, Arc::new(agg)).unwrap();
    let res = registry
        .dispatch_tool_call("run", "tc_1".into(), "web_search", serde_json::json!({"query":"茅台"}))
        .await
        .unwrap();
    assert!(!res.is_error, "{:?}", res.output_summary);
    let results = res.output_summary["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["url"], "https://a.com/maotai");
    assert_eq!(results[0]["source"], "a");
    assert!(res.output_summary["providers"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("a")));
}

// ───────────────────────────── live：DuckDuckGo（免费无 key）

#[tokio::test]
#[ignore = "live: 联网真实搜索（DuckDuckGo 免费无 key；数据中心 IP 常被反爬挡，住宅 IP 通常可）"]
async fn web_search_live_free_providers() {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap();
    let agg = WebSearchConfig { enable_duckduckgo: true, enable_sogou: true }.build(client);
    println!("\n=== web_search live providers: {:?} ===", agg.provider_names());
    let results = agg.search("贵州茅台 2024 业绩", 5).await;
    println!("聚合 {} 条：", results.len());
    for x in &results {
        println!("  [{}] {} — {}", x.source, x.title, x.url);
    }
    // DuckDuckGo best-effort（反爬与 IP 有关）→ 不强制断言非空，仅打印观察。
    eprintln!("[info] DuckDuckGo best-effort；{} 条。", results.len());
}
