//! Canonical URL — 稳定 ID 和 ArticleContent 主键的统一形式。
//!
//! Spec: docs/design/news-module.md §2 (稳定 ID 规则)
//!
//! 规则：
//! - scheme / host 小写、去除 fragment、去除默认端口
//! - 路径去除重复斜杠和尾部斜杠差异
//! - query 参数按 key 排序
//! - tracking query 必须去除：`utm_*`、`spm`、`from`、`source`、`ref`、`refer`、`share`、
//!   `isappinstalled`、`nsukey`

use std::fmt;

/// Tracking query keys 必须去除的最小默认集合（spec §2）。
const DEFAULT_TRACKING_KEYS: &[&str] = &[
    "spm",
    "from",
    "source",
    "ref",
    "refer",
    "share",
    "isappinstalled",
    "nsukey",
];

/// 是否为 tracking key：`utm_*` 或在 [`DEFAULT_TRACKING_KEYS`] 中。
fn is_tracking_key(key: &str) -> bool {
    let lk = key.to_ascii_lowercase();
    if lk.starts_with("utm_") {
        return true;
    }
    DEFAULT_TRACKING_KEYS.iter().any(|k| *k == lk)
}

/// URL canonicalize 失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalUrlError {
    Empty,
    InvalidScheme(String),
    Malformed(String),
}

impl fmt::Display for CanonicalUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "url is empty"),
            Self::InvalidScheme(s) => write!(f, "url scheme not http(s): {}", s),
            Self::Malformed(s) => write!(f, "url malformed: {}", s),
        }
    }
}

impl std::error::Error for CanonicalUrlError {}

/// Canonicalize URL per spec §2。
///
/// 失败：空、缺 scheme、缺 host、scheme 非 http/https。
pub fn canonicalize_url(raw: &str) -> Result<String, CanonicalUrlError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(CanonicalUrlError::Empty);
    }

    // 拆 fragment
    let without_fragment = match trimmed.split_once('#') {
        Some((head, _)) => head,
        None => trimmed,
    };

    // 拆 query
    let (path_part, query_part) = match without_fragment.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (without_fragment, None),
    };

    // scheme://authority/path
    let scheme_end = path_part
        .find("://")
        .ok_or_else(|| CanonicalUrlError::InvalidScheme(path_part.to_string()))?;
    let scheme = path_part[..scheme_end].to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(CanonicalUrlError::InvalidScheme(scheme));
    }
    let rest = &path_part[scheme_end + 3..];
    if rest.is_empty() {
        return Err(CanonicalUrlError::Malformed(raw.to_string()));
    }

    // authority 与 path 切分
    let (authority, raw_path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(CanonicalUrlError::Malformed(raw.to_string()));
    }

    // userinfo@host:port
    let host_port = match authority.rfind('@') {
        Some(idx) => &authority[idx + 1..],
        None => authority,
    };
    let (host, port) = match host_port.rfind(':') {
        Some(idx) => {
            let h = &host_port[..idx];
            let p = &host_port[idx + 1..];
            (h, Some(p))
        }
        None => (host_port, None),
    };
    if host.is_empty() {
        return Err(CanonicalUrlError::Malformed(raw.to_string()));
    }
    let host_lc = host.to_ascii_lowercase();
    let port_canonical = match port {
        Some("") | None => None,
        Some(p) => {
            let is_default = (scheme == "http" && p == "80") || (scheme == "https" && p == "443");
            if is_default {
                None
            } else {
                Some(p.to_string())
            }
        }
    };

    // path normalization — 折叠重复 `/`，保留尾部斜杠差异为"无尾斜杠"
    let normalized_path = normalize_path(raw_path);

    // query normalization
    let normalized_query = match query_part {
        Some(q) if !q.is_empty() => normalize_query(q),
        _ => String::new(),
    };

    let mut out = String::with_capacity(raw.len());
    out.push_str(&scheme);
    out.push_str("://");
    out.push_str(&host_lc);
    if let Some(p) = port_canonical {
        out.push(':');
        out.push_str(&p);
    }
    out.push_str(&normalized_path);
    if !normalized_query.is_empty() {
        out.push('?');
        out.push_str(&normalized_query);
    }
    Ok(out)
}

fn normalize_path(path: &str) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    let mut out = String::with_capacity(path.len() + 1);
    let mut last_was_slash = false;
    for ch in path.chars() {
        if ch == '/' {
            if last_was_slash {
                continue;
            }
            last_was_slash = true;
            out.push('/');
        } else {
            last_was_slash = false;
            out.push(ch);
        }
    }
    // 去掉尾部斜杠（保留根）
    if out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

fn normalize_query(q: &str) -> String {
    let mut pairs: Vec<(String, Option<String>)> = q
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (pair.to_string(), None),
        })
        .filter(|(k, _)| !is_tracking_key(k))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs
        .into_iter()
        .map(|(k, v)| match v {
            Some(v) => format!("{}={}", k, v),
            None => k,
        })
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_fragment_and_lowercases_scheme_host() {
        let u =
            canonicalize_url("HTTPS://Example.com:443/Foo/Bar/#frag").unwrap();
        assert_eq!(u, "https://example.com/Foo/Bar");
    }

    #[test]
    fn removes_default_port() {
        let u = canonicalize_url("http://example.com:80/").unwrap();
        assert_eq!(u, "http://example.com/");
    }

    #[test]
    fn keeps_non_default_port() {
        let u = canonicalize_url("http://example.com:8080/x").unwrap();
        assert_eq!(u, "http://example.com:8080/x");
    }

    #[test]
    fn folds_duplicate_slashes() {
        let u = canonicalize_url("https://a.com///b//c/").unwrap();
        assert_eq!(u, "https://a.com/b/c");
    }

    #[test]
    fn sorts_query_and_removes_tracking() {
        let u = canonicalize_url(
            "https://a.com/p?utm_source=x&b=2&a=1&spm=zz&from=share",
        )
        .unwrap();
        assert_eq!(u, "https://a.com/p?a=1&b=2");
    }

    #[test]
    fn rejects_empty_and_non_http() {
        assert!(canonicalize_url("").is_err());
        assert!(canonicalize_url("ftp://x").is_err());
        assert!(canonicalize_url("not-a-url").is_err());
    }

    #[test]
    fn idempotent() {
        let once = canonicalize_url("HTTPS://Example.com/Foo/?b=2&a=1#x").unwrap();
        let twice = canonicalize_url(&once).unwrap();
        assert_eq!(once, twice);
    }
}
