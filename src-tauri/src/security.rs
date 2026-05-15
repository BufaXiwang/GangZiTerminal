use std::net::IpAddr;

/// 单次外部抓取响应的硬上限——防止上游返回超大文件压爆内存。
pub const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024; // 8 MB

/// 网络抓取前置守门：拒绝 SSRF 风险目标。
/// - 仅允许 http / https
/// - 拒绝 localhost / 私网 / 链路本地 / 广播 / 未指定地址
/// - 拒绝缺 host 的 URL
pub fn validate_external_url(raw: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(raw).map_err(|err| format!("无效 URL：{err}"))?;

    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!("不允许的协议 {scheme}（仅允许 http/https）"));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| "URL 缺少 host".to_string())?;
    if host.is_empty() {
        return Err("URL host 为空".to_string());
    }

    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") {
        return Err("禁止访问 localhost".to_string());
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        if ip.is_loopback() || ip.is_unspecified() {
            return Err(format!("禁止访问 {ip}（loopback/未指定）"));
        }
        match ip {
            IpAddr::V4(v4) => {
                if v4.is_private() || v4.is_link_local() || v4.is_broadcast() {
                    return Err(format!("禁止访问 {v4}（内网/链路本地/广播）"));
                }
            }
            IpAddr::V6(_) => {
                // 简单处理：v6 仅查 loopback/unspecified（已覆盖）
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_javascript_scheme() {
        assert!(validate_external_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn rejects_localhost() {
        assert!(validate_external_url("http://localhost/foo").is_err());
        assert!(validate_external_url("http://127.0.0.1:8080/").is_err());
        assert!(validate_external_url("http://0.0.0.0/").is_err());
    }

    #[test]
    fn rejects_private_ip() {
        assert!(validate_external_url("http://10.0.0.1/").is_err());
        assert!(validate_external_url("http://192.168.1.1/").is_err());
        assert!(validate_external_url("http://172.16.0.1/").is_err());
        assert!(validate_external_url("http://169.254.169.254/").is_err());
    }

    #[test]
    fn allows_public_https() {
        assert!(validate_external_url("https://example.com/foo").is_ok());
        assert!(validate_external_url("https://news.example.cn/article/123").is_ok());
    }
}
