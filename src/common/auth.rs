//! 公共认证工具函数

use std::net::SocketAddr;

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{HeaderMap, Request, header},
};
use subtle::ConstantTimeEq;

/// 提取客户端 IP，用于请求日志。
///
/// 反向代理（nginx / Cloudflare / Caddy）后面拿到的 TCP 对端是代理自身，真实客户端
/// 只能从代理注入的头里读，按常见优先级依次尝试：
/// 1. `X-Forwarded-For` 最左一跳（离客户端最近）
/// 2. `X-Real-IP`
/// 3. `CF-Connecting-IP`
/// 4. TCP 对端地址（直连部署，或代理没注入任何头）
///
/// 这是审计字段而非安全边界：直连部署下客户端可以伪造转发头。
/// 若需要严格取值，应在反向代理层覆写这些头。
pub fn extract_client_ip(request: &Request<Body>) -> Option<String> {
    if let Some(ip) = client_ip_from_headers(request.headers()) {
        return Some(ip);
    }
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip().to_string())
}

fn client_ip_from_headers(headers: &HeaderMap) -> Option<String> {
    let first_hop = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(normalize_ip)
    };
    first_hop("x-forwarded-for")
        .or_else(|| first_hop("x-real-ip"))
        .or_else(|| first_hop("cf-connecting-ip"))
}

/// 去掉偶尔被代理带上的端口（`1.2.3.4:5678`）与 IPv6 方括号（`[::1]:5678`），
/// 让同一客户端在不同连接上落成同一个值，便于按 IP 筛选。
fn normalize_ip(raw: &str) -> String {
    if let Ok(addr) = raw.parse::<SocketAddr>() {
        return addr.ip().to_string();
    }
    if let Ok(ip) = raw.parse::<std::net::IpAddr>() {
        return ip.to_string();
    }
    raw.trim_matches(|c| c == '[' || c == ']').to_string()
}

/// 从请求中提取 API Key
///
/// 支持两种认证方式：
/// - `x-api-key` header（空值视为未提供，回退到 Authorization）
/// - `Authorization: Bearer <token>` header（scheme 大小写不敏感，见 RFC 7235）
pub fn extract_api_key(request: &Request<Body>) -> Option<String> {
    // 优先检查 x-api-key
    if let Some(key) = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .filter(|k| !k.is_empty())
    {
        return Some(key.to_string());
    }

    // 其次检查 Authorization: Bearer
    request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .and_then(|(scheme, token)| {
            scheme
                .eq_ignore_ascii_case("bearer")
                .then(|| token.to_string())
        })
}

/// 常量时间字符串比较，防止时序攻击
///
/// 无论字符串内容如何，比较所需的时间都是恒定的，
/// 这可以防止攻击者通过测量响应时间来猜测登录API密钥。
///
/// 使用经过安全审计的 `subtle` crate 实现
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn req_with(headers: &[(&'static str, &str)], peer: Option<&str>) -> Request<Body> {
        let mut req = Request::builder().uri("/").body(Body::empty()).unwrap();
        for (k, v) in headers {
            req.headers_mut()
                .insert(*k, HeaderValue::from_str(v).unwrap());
        }
        if let Some(p) = peer {
            req.extensions_mut()
                .insert(ConnectInfo(p.parse::<SocketAddr>().unwrap()));
        }
        req
    }

    #[test]
    fn prefers_leftmost_forwarded_for() {
        let req = req_with(
            &[("x-forwarded-for", "203.0.113.9, 10.0.0.1, 10.0.0.2")],
            Some("10.0.0.2:4567"),
        );
        assert_eq!(extract_client_ip(&req).as_deref(), Some("203.0.113.9"));
    }

    #[test]
    fn falls_back_through_real_ip_and_cf_then_peer() {
        let req = req_with(&[("x-real-ip", "198.51.100.7")], Some("10.0.0.2:4567"));
        assert_eq!(extract_client_ip(&req).as_deref(), Some("198.51.100.7"));

        let req = req_with(&[("cf-connecting-ip", "192.0.2.33")], Some("10.0.0.2:4567"));
        assert_eq!(extract_client_ip(&req).as_deref(), Some("192.0.2.33"));

        let req = req_with(&[], Some("10.0.0.2:4567"));
        assert_eq!(extract_client_ip(&req).as_deref(), Some("10.0.0.2"));

        let req = req_with(&[], None);
        assert_eq!(extract_client_ip(&req), None);
    }

    #[test]
    fn strips_port_and_brackets() {
        let req = req_with(&[("x-forwarded-for", "203.0.113.9:51234")], None);
        assert_eq!(extract_client_ip(&req).as_deref(), Some("203.0.113.9"));

        let req = req_with(&[("x-forwarded-for", "[2001:db8::1]:443")], None);
        assert_eq!(extract_client_ip(&req).as_deref(), Some("2001:db8::1"));

        let req = req_with(&[("x-forwarded-for", "2001:db8::1")], None);
        assert_eq!(extract_client_ip(&req).as_deref(), Some("2001:db8::1"));
    }

    #[test]
    fn ignores_empty_forwarded_header() {
        let req = req_with(&[("x-forwarded-for", " ")], Some("10.0.0.2:4567"));
        assert_eq!(extract_client_ip(&req).as_deref(), Some("10.0.0.2"));
    }

    #[test]
    fn extracts_bearer_case_insensitively() {
        for auth in ["Bearer sk-1", "bearer sk-1", "BEARER sk-1"] {
            let req = req_with(&[("authorization", auth)], None);
            assert_eq!(extract_api_key(&req).as_deref(), Some("sk-1"), "{auth}");
        }
        let req = req_with(&[("authorization", "Basic c2stMQ==")], None);
        assert_eq!(extract_api_key(&req), None);
        let req = req_with(&[("authorization", "Bearer")], None);
        assert_eq!(extract_api_key(&req), None);
    }

    #[test]
    fn x_api_key_preferred_and_empty_falls_back() {
        let req = req_with(
            &[("x-api-key", "sk-x"), ("authorization", "Bearer sk-b")],
            None,
        );
        assert_eq!(extract_api_key(&req).as_deref(), Some("sk-x"));

        let req = req_with(&[("x-api-key", ""), ("authorization", "Bearer sk-b")], None);
        assert_eq!(extract_api_key(&req).as_deref(), Some("sk-b"));

        let req = req_with(&[("x-api-key", "")], None);
        assert_eq!(extract_api_key(&req), None);
    }
}
