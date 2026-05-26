//! URL canonicalization —— spec `news-module.md §2`。
//!
//! 至少包括：scheme / host 小写、去 fragment、去默认端口、路径去重复斜杠和
//! 尾部斜杠差异、query 参数按 key 排序，去除 utm_* / spm / from / source /
//! ref / refer / share / isappinstalled / nsukey。

const TRACKING_PARAMS: &[&str] = &[
    "spm",
    "from",
    "source",
    "ref",
    "refer",
    "share",
    "isappinstalled",
    "nsukey",
];

/// 把 provider 原始 URL 归一化为 canonical URL（spec §2 稳定 ID 派生输入）。
///
/// 输入非合法 URL 时原样 trim 后返回，调用方按 `not_resolvable` 处理。
pub fn canonical_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // 简化处理：手工分段拆解，避免引入新依赖。
    // 1. fragment
    let no_fragment = match trimmed.find('#') {
        Some(idx) => &trimmed[..idx],
        None => trimmed,
    };
    // 2. scheme
    let (scheme, rest) = match no_fragment.find("://") {
        Some(idx) => (&no_fragment[..idx], &no_fragment[idx + 3..]),
        None => return no_fragment.to_string(),
    };
    let scheme_lc = scheme.to_lowercase();

    // 3. host + path + query
    let (authority, path_and_query) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };
    let (host_raw, port_opt) = match authority.rfind(':') {
        Some(idx) if !authority.contains('@') => {
            (&authority[..idx], Some(&authority[idx + 1..]))
        }
        _ => (authority, None),
    };
    let host_lc = host_raw.to_lowercase();

    // 4. 去默认端口
    let port_part = match (scheme_lc.as_str(), port_opt) {
        ("http", Some("80")) | ("https", Some("443")) => String::new(),
        (_, Some(p)) => format!(":{p}"),
        (_, None) => String::new(),
    };

    // 5. path + query
    let (mut path, query) = match path_and_query.find('?') {
        Some(idx) => (
            path_and_query[..idx].to_string(),
            Some(&path_and_query[idx + 1..]),
        ),
        None => (path_and_query.to_string(), None),
    };
    if path.is_empty() {
        path = "/".to_string();
    }
    // 路径去重复斜杠
    let collapsed = collapse_slashes(&path);
    let normalized_path = trim_trailing_slash(&collapsed);

    // 6. query：按 key 排序 + 去 tracking
    let normalized_query = match query {
        Some(q) if !q.is_empty() => {
            // 保留原始 key 大小写参与最终 URL（spec §2 只要求按 key 排序 / 去 tracking）
            let mut pairs: Vec<(String, String)> = q
                .split('&')
                .filter(|p| !p.is_empty())
                .map(|kv| {
                    let mut it = kv.splitn(2, '=');
                    let k = it.next().unwrap_or("").to_string();
                    let v = it.next().unwrap_or("").to_string();
                    (k, v)
                })
                .filter(|(k, _)| !is_tracking_key(&k.to_lowercase()))
                .collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            if pairs.is_empty() {
                String::new()
            } else {
                let joined: Vec<String> = pairs
                    .into_iter()
                    .map(|(k, v)| {
                        if v.is_empty() {
                            k
                        } else {
                            format!("{k}={v}")
                        }
                    })
                    .collect();
                format!("?{}", joined.join("&"))
            }
        }
        _ => String::new(),
    };

    format!("{scheme_lc}://{host_lc}{port_part}{normalized_path}{normalized_query}")
}

fn is_tracking_key(key: &str) -> bool {
    if key.starts_with("utm_") {
        return true;
    }
    TRACKING_PARAMS.contains(&key)
}

fn collapse_slashes(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut last_slash = false;
    for c in path.chars() {
        if c == '/' {
            if !last_slash {
                out.push(c);
            }
            last_slash = true;
        } else {
            out.push(c);
            last_slash = false;
        }
    }
    out
}

fn trim_trailing_slash(path: &str) -> String {
    if path.len() > 1 && path.ends_with('/') {
        path.trim_end_matches('/').to_string()
    } else {
        path.to_string()
    }
}

/// 稳定 ID 派生 —— spec §2 三规则：
/// 1. 有 URL → `Ok("{source}:url:{sha256(canonical_url)}")`
/// 2. 无 URL，provider 提供稳定 item id / guid → `Ok("{source}:item:{sha256(item_id)}")`
/// 3. 都没有，但有 title / published / summary → `Ok("{source}:fingerprint:{sha256(...)}")`
/// 4. 三者都空 → `Err`，调用方必须跳过该 item 并写 refresh warning（spec §2）。
/// hash 算法固定为 SHA-256（spec），输出 64 字符 lowercase hex，不截断。
pub fn stable_news_id(
    source: &str,
    url: Option<&str>,
    item_id: Option<&str>,
) -> Result<String, NoStableIdError> {
    if let Some(u) = url.filter(|s| !s.is_empty()) {
        let c = canonical_url(u);
        if !c.is_empty() {
            return Ok(format!("{source}:url:{}", hash_hex(&c)));
        }
    }
    if let Some(i) = item_id.filter(|s| !s.is_empty()) {
        return Ok(format!("{source}:item:{}", hash_hex(i)));
    }
    Err(NoStableIdError)
}

/// 进阶 fingerprint 派生：spec §2 第三规则要求至少一个稳定字段参与；全空必须 Err。
pub fn fingerprint_news_id(
    source: &str,
    normalized_title: Option<&str>,
    normalized_published: Option<&str>,
    normalized_summary: Option<&str>,
) -> Result<String, NoStableIdError> {
    let title = normalized_title.unwrap_or("");
    let published = normalized_published.unwrap_or("");
    let summary = normalized_summary.unwrap_or("");
    if title.is_empty() && published.is_empty() && summary.is_empty() {
        return Err(NoStableIdError);
    }
    let raw = format!("{title}|{published}|{summary}");
    Ok(format!("{source}:fingerprint:{}", hash_hex(&raw)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoStableIdError;

impl std::fmt::Display for NoStableIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no stable id available (need url, item_id, or fingerprint inputs)")
    }
}

impl std::error::Error for NoStableIdError {}

fn hash_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let digest = h.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest.iter() {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercases_scheme_and_host() {
        assert_eq!(
            canonical_url("HTTPS://Example.com/Path"),
            "https://example.com/Path"
        );
    }

    #[test]
    fn strips_fragment() {
        assert_eq!(
            canonical_url("https://example.com/a#section"),
            "https://example.com/a"
        );
    }

    #[test]
    fn drops_default_ports() {
        assert_eq!(canonical_url("https://x.com:443/a"), "https://x.com/a");
        assert_eq!(canonical_url("http://x.com:80/a"), "http://x.com/a");
    }

    #[test]
    fn strips_tracking_params() {
        assert_eq!(
            canonical_url("https://x.com/a?utm_source=x&id=42&spm=abc"),
            "https://x.com/a?id=42"
        );
    }

    #[test]
    fn sorts_query_keys() {
        assert_eq!(
            canonical_url("https://x.com/a?b=2&a=1"),
            "https://x.com/a?a=1&b=2"
        );
    }

    #[test]
    fn trims_trailing_slash() {
        assert_eq!(canonical_url("https://x.com/a/"), "https://x.com/a");
        assert_eq!(canonical_url("https://x.com/"), "https://x.com/");
    }

    #[test]
    fn stable_id_uses_canonical_url() {
        let id1 = stable_news_id("cls:hot", Some("https://Example.com/a?utm_source=x"), None)
            .expect("id");
        let id2 = stable_news_id("cls:hot", Some("https://example.com/a"), None).expect("id");
        assert_eq!(id1, id2);
        assert!(id1.starts_with("cls:hot:url:"));
    }

    #[test]
    fn stable_id_errors_on_no_url_no_item_id() {
        let r = stable_news_id("rss:x", None, None);
        assert!(r.is_err());
    }

    #[test]
    fn fingerprint_requires_at_least_one_field() {
        assert!(fingerprint_news_id("rss:x", None, None, None).is_err());
        assert!(fingerprint_news_id("rss:x", Some(""), Some(""), Some("")).is_err());
        let a = fingerprint_news_id("rss:x", Some("某新闻"), Some("2026-01-01"), None).unwrap();
        let b = fingerprint_news_id("rss:x", Some("另一条"), Some("2026-01-01"), None).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn tracking_params_full_set_dropped() {
        let cases = [
            ("from", "wx"),
            ("ref", "feed"),
            ("refer", "page"),
            ("share", "twitter"),
            ("isappinstalled", "0"),
            ("nsukey", "abc"),
        ];
        for (k, v) in &cases {
            let raw = format!("https://x.com/a?id=1&{k}={v}");
            let cleaned = canonical_url(&raw);
            assert!(
                !cleaned.contains(&format!("{k}=")),
                "tracking key {k} 应被去除: {cleaned}",
            );
        }
    }
}
