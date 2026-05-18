use crate::infrastructure::news::article::cleanup::{
    dedupe, is_content_text, normalize_text, parse_selector,
};
use crate::infrastructure::news::article::model::ExtractContext;
use scraper::Html;
use serde_json::Value;

pub fn extract_title(document: &Html) -> Option<String> {
    extract_meta(
        document,
        &[
            ("property", "og:title"),
            ("name", "twitter:title"),
            ("name", "title"),
        ],
    )
    .or_else(|| extract_json_ld_string(document, &["headline", "title"]))
    .or_else(|| text_from_first(document, "h1"))
    .or_else(|| text_from_first(document, "title"))
}

pub fn extract_author(document: &Html) -> Option<String> {
    extract_meta(
        document,
        &[
            ("name", "author"),
            ("property", "article:author"),
            ("name", "weibo:article:create_at"),
        ],
    )
    .or_else(|| extract_json_ld_author(document))
}

pub fn extract_published(document: &Html) -> Option<String> {
    extract_meta(
        document,
        &[
            ("property", "article:published_time"),
            ("name", "pubdate"),
            ("name", "publishdate"),
            ("name", "date"),
        ],
    )
    .or_else(|| extract_json_ld_string(document, &["datePublished", "dateCreated"]))
}

pub fn extract_description(document: &Html) -> Option<String> {
    extract_meta(
        document,
        &[
            ("property", "og:description"),
            ("name", "twitter:description"),
            ("name", "description"),
        ],
    )
    .or_else(|| extract_json_ld_string(document, &["articleBody", "description"]))
}

pub fn extract_embedded_paragraphs(document: &Html, context: &ExtractContext<'_>) -> Vec<String> {
    let values = json_script_values(document);
    let mut candidates = Vec::new();

    for value in values {
        collect_content_strings(&value, &mut candidates);
    }

    if let Some(title) = context.fallback_title {
        candidates.sort_by_key(|text| if text.contains(title) { 0 } else { 1 });
    }

    dedupe(
        candidates
            .into_iter()
            .flat_map(|text| split_article_text(&text))
            .filter(|text| is_content_text(text))
            .collect(),
    )
}

pub fn extract_images(document: &Html, base_url: &str) -> Vec<String> {
    parse_selector(
        "article img, main img, .article-content img, .content img, [class*='article'] img",
    )
    .map(|selector| {
        document
            .select(&selector)
            .filter_map(|node| {
                node.value()
                    .attr("src")
                    .or_else(|| node.value().attr("data-src"))
                    .or_else(|| node.value().attr("data-original"))
            })
            .filter_map(|src| resolve_url(base_url, src))
            .take(6)
            .collect()
    })
    .unwrap_or_default()
}

fn extract_meta(document: &Html, attrs: &[(&str, &str)]) -> Option<String> {
    for (attr, value) in attrs {
        let selector = format!("meta[{attr}=\"{value}\"]");
        if let Some(selector) = parse_selector(&selector) {
            if let Some(content) = document
                .select(&selector)
                .find_map(|node| node.value().attr("content"))
            {
                if let Some(text) = normalize_text(content) {
                    return Some(text);
                }
            }
        }
    }
    None
}

fn text_from_first(document: &Html, selector: &str) -> Option<String> {
    parse_selector(selector)
        .and_then(|selector| document.select(&selector).next())
        .and_then(|element| normalize_text(&element.text().collect::<Vec<_>>().join("")))
}

fn extract_json_ld_string(document: &Html, keys: &[&str]) -> Option<String> {
    for value in json_ld_values(document) {
        if let Some(text) = string_from_json_keys(&value, keys) {
            return Some(text);
        }
    }
    None
}

fn extract_json_ld_author(document: &Html) -> Option<String> {
    for value in json_ld_values(document) {
        let Some(author) = value.get("author") else {
            continue;
        };
        if let Some(text) = author.as_str().and_then(normalize_text) {
            return Some(text);
        }
        if let Some(text) = author
            .get("name")
            .and_then(Value::as_str)
            .and_then(normalize_text)
        {
            return Some(text);
        }
        if let Some(items) = author.as_array() {
            for item in items {
                if let Some(text) = item
                    .get("name")
                    .and_then(Value::as_str)
                    .and_then(normalize_text)
                {
                    return Some(text);
                }
            }
        }
    }
    None
}

fn json_ld_values(document: &Html) -> Vec<Value> {
    let Some(selector) = parse_selector(r#"script[type="application/ld+json"]"#) else {
        return Vec::new();
    };
    document
        .select(&selector)
        .flat_map(|node| {
            let raw = node.text().collect::<Vec<_>>().join("");
            let Ok(value) = serde_json::from_str::<Value>(&raw) else {
                return Vec::new();
            };
            match value {
                Value::Array(items) => items,
                value => vec![value],
            }
        })
        .collect()
}

fn json_script_values(document: &Html) -> Vec<Value> {
    let Some(selector) = parse_selector("script") else {
        return Vec::new();
    };

    document
        .select(&selector)
        .filter_map(|node| {
            let raw = node.text().collect::<Vec<_>>().join("");
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return None;
            }

            if trimmed.starts_with('{') || trimmed.starts_with('[') {
                return serde_json::from_str::<Value>(trimmed).ok();
            }

            extract_json_assignment(trimmed)
        })
        .flat_map(|value| match value {
            Value::Array(items) => items,
            value => vec![value],
        })
        .collect()
}

fn extract_json_assignment(script: &str) -> Option<Value> {
    let start = script.find('{')?;
    let end = script.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Value>(&script[start..=end]).ok()
}

fn collect_content_strings(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if content_key(key) {
                    if let Some(text) = value.as_str().and_then(normalize_text) {
                        output.push(clean_embedded_text(&text));
                    }
                }
                collect_content_strings(value, output);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_content_strings(item, output);
            }
        }
        _ => {}
    }
}

fn content_key(key: &str) -> bool {
    matches!(
        key,
        "articleBody"
            | "content"
            | "contentHtml"
            | "articleContent"
            | "detail"
            | "body"
            | "summary"
            | "description"
            | "brief"
    )
}

fn split_article_text(text: &str) -> Vec<String> {
    clean_embedded_text(text)
        .split('\n')
        .filter_map(normalize_text)
        .collect()
}

fn clean_embedded_text(text: &str) -> String {
    let mut output = String::new();
    let mut in_tag = false;
    let mut last_was_newline = false;

    for ch in text
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n")
        .replace("</p>", "\n")
        .replace("</div>", "\n")
        .chars()
    {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => {
                if ch == '\n' {
                    if !last_was_newline {
                        output.push(ch);
                    }
                    last_was_newline = true;
                } else {
                    output.push(ch);
                    last_was_newline = false;
                }
            }
            _ => {}
        }
    }

    output
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn string_from_json_keys(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(text) = value
            .get(*key)
            .and_then(Value::as_str)
            .and_then(normalize_text)
        {
            return Some(text);
        }
    }
    if let Some(graph) = value.get("@graph").and_then(Value::as_array) {
        for item in graph {
            if let Some(text) = string_from_json_keys(item, keys) {
                return Some(text);
            }
        }
    }
    None
}

fn resolve_url(base_url: &str, src: &str) -> Option<String> {
    if src.starts_with("http://") || src.starts_with("https://") {
        return Some(src.to_string());
    }
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|base| base.join(src).ok())
        .map(|url| url.to_string())
}
