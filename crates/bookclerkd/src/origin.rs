//! Public origin used for OIDC issuer, SSO callbacks, and invite URLs.
//!
//! Prefers `integrations.public_origin`. When that pin is unset, only a
//! **bound loopback** `Origin` / `Host` (matching `daemon.listen` ports) is
//! accepted — never an arbitrary hostname, and never `X-Forwarded-*` /
//! `Forwarded` / `Via` / `X-Real-IP`. Non-loopback operation must set
//! `public_origin`; forwarded headers are for login throttling via
//! `daemon.trusted_proxies`, not for minting issuers.

use std::net::IpAddr;
use std::str::FromStr;

use axum::http::uri::Authority;
use axum::http::{header, HeaderMap};
use bookclerk_config::{Config, ListenAddrs};
use url::Url;

/// Trimmed configured origin with no trailing slash, or `None` when unset.
#[must_use]
pub fn configured_public_origin(public_origin: Option<&str>) -> Option<String> {
    public_origin
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
}

/// Rewrite loopback IPs (`127.0.0.1`, `::1`) to `localhost`, keeping scheme/port.
#[must_use]
pub fn rewrite_loopback_host(origin: &str) -> String {
    let trimmed = origin.trim().trim_end_matches('/');
    let Ok(url) = Url::parse(trimmed) else {
        return trimmed.to_string();
    };
    let loopback = match url.host() {
        Some(url::Host::Ipv4(addr)) if addr.is_loopback() => true,
        Some(url::Host::Ipv6(addr)) if addr.is_loopback() => true,
        _ => false,
    };
    if !loopback {
        return trimmed.to_string();
    }
    match url.port() {
        Some(port) => format!("{}://localhost:{port}", url.scheme()),
        None => format!("{}://localhost", url.scheme()),
    }
}

/// Request `Origin` when it is a usable absolute origin (not `null`).
#[must_use]
pub fn request_origin(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::ORIGIN)?.to_str().ok()?.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("null") {
        return None;
    }
    Some(raw.trim_end_matches('/').to_string())
}

/// True when a reverse-proxy forwarding header name is present, regardless of value.
fn has_forwarded_headers(headers: &HeaderMap) -> bool {
    headers.keys().any(|name| {
        let n = name.as_str();
        n == "forwarded" || n == "via" || n == "x-real-ip" || n.starts_with("x-forwarded-")
    })
}

/// Hostname is localhost / 127.0.0.1 / ::1 (no leftover suffix on the authority).
fn exact_host_is_loopback(raw: &str) -> bool {
    if let Some(rest) = raw.strip_prefix('[') {
        let Some((inside, after)) = rest.split_once(']') else {
            return false;
        };
        if !after.is_empty() {
            let Some(port) = after.strip_prefix(':') else {
                return false;
            };
            if port.parse::<u16>().is_err() {
                return false;
            }
        }
        return inside.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    }
    let Ok(authority) = Authority::from_str(raw) else {
        return false;
    };
    if authority.as_str() != raw {
        return false;
    }
    if raw.rsplit_once(':').is_some_and(|(_, p)| !p.is_empty()) && authority.port_u16().is_none() {
        return false;
    }
    let hostname = authority.host();
    hostname.eq_ignore_ascii_case("localhost")
        || hostname == "127.0.0.1"
        || hostname.eq_ignore_ascii_case("::1")
}

/// Port from a loopback `Host` / origin authority (`80` when omitted).
fn loopback_port_from_authority(raw: &str) -> Option<u16> {
    if !exact_host_is_loopback(raw) {
        return None;
    }
    if let Some(rest) = raw.strip_prefix('[') {
        let (_, after) = rest.split_once(']')?;
        return if after.is_empty() {
            Some(80)
        } else {
            after.strip_prefix(':')?.parse().ok()
        };
    }
    let authority = Authority::from_str(raw).ok()?;
    Some(authority.port_u16().unwrap_or(80))
}

/// `http://localhost:<port>` when `authority` is loopback on a bound listen port.
fn bound_loopback_http_origin(authority: &str, listen: &ListenAddrs) -> Option<String> {
    let port = loopback_port_from_authority(authority)?;
    if !listen.ports().contains(&port) {
        return None;
    }
    Some(format!("http://localhost:{port}"))
}

/// Bound-loopback origin from `Origin`, if it is `http` and matches `daemon.listen`.
fn origin_header_if_bound_loopback(headers: &HeaderMap, listen: &ListenAddrs) -> Option<String> {
    let raw = request_origin(headers)?;
    let url = Url::parse(&raw).ok()?;
    if url.scheme() != "http" {
        return None;
    }
    let host = url.host_str()?;
    let port = url.port_or_known_default()?;
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    bound_loopback_http_origin(&authority, listen)
}

/// Bound-loopback origin from a single well-formed `Host` header.
fn host_header_if_bound_loopback(headers: &HeaderMap, listen: &ListenAddrs) -> Option<String> {
    let mut hosts = headers.get_all(header::HOST).iter();
    let host = hosts.next()?;
    if hosts.next().is_some() {
        return None;
    }
    let raw = host.to_str().ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    bound_loopback_http_origin(raw, listen)
}

/// Loopback issuer when nothing is configured or detected.
#[must_use]
pub fn loopback_fallback(listen: &ListenAddrs) -> String {
    format!("http://localhost:{}", listen.ui_port())
}

/// Origin detected from this request, rewritten to `localhost` on loopback IPs.
///
/// Ignores `Origin` / `Host` when any forwarding header is present, or when the
/// authority is not a bound loopback listen port.
#[must_use]
pub fn detected_origin(headers: &HeaderMap, listen: &ListenAddrs) -> String {
    if has_forwarded_headers(headers) {
        return loopback_fallback(listen);
    }
    origin_header_if_bound_loopback(headers, listen)
        .or_else(|| host_header_if_bound_loopback(headers, listen))
        .unwrap_or_else(|| loopback_fallback(listen))
}

/// Effective public origin: pinned config, else bound loopback request, else localhost.
#[must_use]
pub fn effective_public_origin(
    public_origin: Option<&str>,
    headers: Option<&HeaderMap>,
    listen: &ListenAddrs,
) -> String {
    if let Some(configured) = configured_public_origin(public_origin) {
        return rewrite_loopback_host(&configured);
    }
    if let Some(headers) = headers {
        return detected_origin(headers, listen);
    }
    loopback_fallback(listen)
}

/// Effective origin from a live [`Config`] plus optional request headers.
#[must_use]
pub fn effective_origin_from_config(cfg: &Config, headers: Option<&HeaderMap>) -> String {
    effective_public_origin(
        cfg.integrations.public_origin.as_deref(),
        headers,
        &cfg.daemon.listen,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn listen_8787() -> ListenAddrs {
        ListenAddrs::parse_list("127.0.0.1:8787").unwrap()
    }

    #[test]
    fn rewrites_loopback_ip_to_localhost() {
        assert_eq!(
            rewrite_loopback_host("http://127.0.0.1:8787"),
            "http://localhost:8787"
        );
        assert_eq!(
            rewrite_loopback_host("http://[::1]:8787"),
            "http://localhost:8787"
        );
        assert_eq!(
            rewrite_loopback_host("https://bookclerk.example.com"),
            "https://bookclerk.example.com"
        );
    }

    #[test]
    fn pinned_origin_wins_over_request() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://localhost:5173"),
        );
        let listen = listen_8787();
        assert_eq!(
            effective_public_origin(
                Some("https://bookclerk.example.com"),
                Some(&headers),
                &listen
            ),
            "https://bookclerk.example.com"
        );
    }

    #[test]
    fn unset_origin_uses_bound_loopback_then_localhost() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:8787"),
        );
        let listen = listen_8787();
        assert_eq!(
            effective_public_origin(None, Some(&headers), &listen),
            "http://localhost:8787"
        );
        assert_eq!(
            effective_public_origin(None, None, &listen),
            "http://localhost:8787"
        );
    }

    #[test]
    fn hostile_origin_does_not_become_issuer() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        let listen = listen_8787();
        assert_eq!(
            effective_public_origin(None, Some(&headers), &listen),
            "http://localhost:8787"
        );
    }

    #[test]
    fn hostile_host_does_not_become_issuer() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("evil.example"));
        let listen = listen_8787();
        assert_eq!(
            effective_public_origin(None, Some(&headers), &listen),
            "http://localhost:8787"
        );
    }

    #[test]
    fn forwarded_headers_are_ignored_even_with_loopback_host() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("localhost:8787"));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        let listen = listen_8787();
        assert_eq!(
            effective_public_origin(None, Some(&headers), &listen),
            "http://localhost:8787"
        );
    }

    #[test]
    fn unbound_loopback_port_is_ignored() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://localhost:5173"),
        );
        let listen = listen_8787();
        assert_eq!(
            effective_public_origin(None, Some(&headers), &listen),
            "http://localhost:8787"
        );
    }

    #[test]
    fn x_forwarded_host_does_not_override_fallback() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("localhost:8787"));
        headers.insert(
            "x-forwarded-host",
            HeaderValue::from_static("bookclerk.example.com"),
        );
        let listen = listen_8787();
        assert_eq!(
            effective_public_origin(None, Some(&headers), &listen),
            "http://localhost:8787"
        );
    }

    #[test]
    fn via_and_x_real_ip_are_ignored() {
        let listen = listen_8787();
        let mut via = HeaderMap::new();
        via.insert(header::HOST, HeaderValue::from_static("localhost:8787"));
        via.insert(header::VIA, HeaderValue::from_static("1.1 evil.example"));
        assert_eq!(
            effective_public_origin(None, Some(&via), &listen),
            "http://localhost:8787"
        );

        let mut real_ip = HeaderMap::new();
        real_ip.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://localhost:8787"),
        );
        real_ip.insert("x-real-ip", HeaderValue::from_static("203.0.113.9"));
        assert_eq!(
            effective_public_origin(None, Some(&real_ip), &listen),
            "http://localhost:8787"
        );
    }

    #[test]
    fn duplicate_host_and_https_loopback_are_ignored() {
        let listen = listen_8787();
        let mut dup = HeaderMap::new();
        dup.append(header::HOST, HeaderValue::from_static("localhost:8787"));
        dup.append(header::HOST, HeaderValue::from_static("evil.example"));
        assert_eq!(
            effective_public_origin(None, Some(&dup), &listen),
            "http://localhost:8787"
        );

        let mut https = HeaderMap::new();
        https.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://localhost:8787"),
        );
        assert_eq!(
            effective_public_origin(None, Some(&https), &listen),
            "http://localhost:8787"
        );
    }
}
