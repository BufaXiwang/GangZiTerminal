//! `fetch_news` —— News 模块 canonical 读取工具。
//!
//! 对齐 docs/design/agent-runtime-module.md §4 `FetchNewsToolInput`，引用
//! news-module spec 的 `FetchNewsRequest`：ids / query / sources / publishedFrom /
//! publishedTo / includeArticle / limit / offset。

use crate::domain::agent::types::ToolResultContent;
use crate::domain::news::canonical_url::canonical_url;
use crate::infrastructure::news::repository as nrepo;
use crate::pipeline::agent::tools::{err_text, ok_json, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::AppHandle;

const ARTICLE_EXCERPT_MAX_CHARS: usize = 500;

pub struct FetchNewsTool {
    app: AppHandle,
}

impl FetchNewsTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for FetchNewsTool {
    fn name(&self) -> &'static str {
        "fetch_news"
    }

    fn description(&self) -> &'static str {
        "读取本地资讯。支持 ids 精确取、query 全文搜索（标题 / 摘要 / 正文）、\
         sources 过滤、publishedFrom/To 时间窗口、includeArticle 取正文。\
         缺正文时返回 article_missing warning，不触发远端抽取。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ids":        {"type": "array", "items": {"type": "string"}},
                "query":      {"type": "string"},
                "sources":    {"type": "array", "items": {"type": "string"}},
                "publishedFrom": {"type": "string"},
                "publishedTo":   {"type": "string"},
                "includeArticle": {"type": "boolean"},
                "limit":  {"type": "integer", "minimum": 1, "maximum": 200},
                "offset": {"type": "integer", "minimum": 0}
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let limit_in = input
            .get("limit")
            .and_then(Value::as_i64)
            .unwrap_or(50)
            .clamp(1, 200);
        let offset_in = input.get("offset").and_then(Value::as_i64).unwrap_or(0).max(0);
        let opts = nrepo::NewsQueryOpts {
            ids: input.get("ids").and_then(Value::as_array).map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            }),
            query: input.get("query").and_then(Value::as_str).map(String::from),
            sources: input.get("sources").and_then(Value::as_array).map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            }),
            published_from: input
                .get("publishedFrom")
                .and_then(Value::as_str)
                .map(String::from),
            published_to: input
                .get("publishedTo")
                .and_then(Value::as_str)
                .map(String::from),
            // +1 探测 hasMore（spec §4「hasMore 必须基于同一查询条件计算」）
            limit: Some(limit_in + 1),
            offset: Some(offset_in),
        };
        let include_article = input
            .get("includeArticle")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let requested_ids = opts.ids.clone();
        let app = self.app.clone();
        let r = tokio::task::spawn_blocking(move || nrepo::query_news_items(&app, opts)).await;
        let mut rows = match r {
            Ok(Ok(rows)) => rows,
            Ok(Err(msg)) => return err_text(msg),
            Err(e) => return err_text(format!("fetch_news 任务异常：{e}")),
        };
        let has_more = rows.len() as i64 > limit_in;
        if has_more {
            rows.truncate(limit_in as usize);
        }

        // 构造 items + 可选 article + warnings
        let mut items: Vec<Value> = Vec::with_capacity(rows.len());
        for item in &rows {
            let mut value = json!({
                "id": item.id,
                "source": item.source,
                "title": item.title,
                "summary": item.summary,
                "url": item.link,
                "publishedAt": item.published,
            });
            let mut warnings: Vec<&'static str> = Vec::new();
            // articleExcerpt + 可选 article 全文
            let article_payload = item
                .link
                .as_deref()
                .map(|u| canonical_url(u))
                .filter(|u| !u.is_empty())
                .and_then(|u| nrepo::load_article_content_ref(&self.app, &u).ok().flatten());
            let article_content_str = article_payload
                .as_ref()
                .and_then(|v| v.get("content"))
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty());
            if let Some(content) = article_content_str {
                let excerpt = clean_excerpt(content, ARTICLE_EXCERPT_MAX_CHARS);
                value
                    .as_object_mut()
                    .unwrap()
                    .insert("articleExcerpt".into(), Value::String(excerpt));
                if include_article {
                    let article_fetched_at = article_payload
                        .as_ref()
                        .and_then(|v| v.get("fetchedAt"))
                        .cloned()
                        .or_else(|| {
                            article_payload
                                .as_ref()
                                .and_then(|v| v.get("fetched_at"))
                                .cloned()
                        });
                    let article_title = article_payload
                        .as_ref()
                        .and_then(|v| v.get("title"))
                        .cloned();
                    value.as_object_mut().unwrap().insert(
                        "article".into(),
                        json!({
                            "title": article_title,
                            "content": content,
                            "fetchedAt": article_fetched_at,
                        }),
                    );
                }
            } else if include_article {
                warnings.push("article_missing");
            }
            if !warnings.is_empty() {
                value
                    .as_object_mut()
                    .unwrap()
                    .insert(
                        "warnings".into(),
                        Value::Array(warnings.iter().map(|s| json!(s)).collect()),
                    );
            }
            items.push(value);
        }

        // ids 模式下：未命中的 id 进 errors[]，code = not_found
        let mut errors: Vec<Value> = Vec::new();
        if let Some(ids) = requested_ids {
            let returned: std::collections::HashSet<&str> =
                rows.iter().map(|r| r.id.as_str()).collect();
            for id in &ids {
                if !returned.contains(id.as_str()) {
                    errors.push(json!({
                        "id": id,
                        "code": "not_found",
                    }));
                }
            }
        }

        let page = json!({
            "limit": limit_in,
            "offset": offset_in,
            "hasMore": has_more,
        });

        let mut response = json!({
            "snapshotAt": chrono::Utc::now().to_rfc3339(),
            "freshness": {"status": "fresh"},
            "items": items,
            "page": page,
        });
        if !errors.is_empty() {
            response
                .as_object_mut()
                .unwrap()
                .insert("errors".into(), Value::Array(errors));
        }
        (ok_json(response), false)
    }
}

fn clean_excerpt(content: &str, max: usize) -> String {
    // 简单清洗：去多余空白行；按字符数截取。
    let trimmed = content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if trimmed.chars().count() <= max {
        trimmed
    } else {
        let cut: String = trimmed.chars().take(max).collect();
        format!("{cut}…")
    }
}
