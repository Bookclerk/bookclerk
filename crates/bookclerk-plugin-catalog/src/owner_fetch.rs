//! Fetching an archive whose URL an owner cluster chose, on a host node.
//!
//! A host fetches owner `https:` archives itself, outside any plugin jail
//! and network consent, so the URL is held to more than the installer's
//! [`crate::fetch_artifact`]: `https:` only, no credentials in the URL, a
//! host that is not `localhost` or a loopback, private, link-local,
//! unique-local or unspecified IP literal ([`check_owner_artifact_url`]),
//! and every redirect hop checked the same way. A hop may change origin
//! (registry downloads redirect to object storage): the operator approved
//! the URL and the archive digest, which binds the bytes either way.

use std::io::Read;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;

use url::{Host, Url};

use crate::error::{CatalogError, Result};
use crate::extract::write_file;
use crate::install::{DOWNLOAD_TIMEOUT, MAX_DOWNLOAD_BYTES, MAX_REDIRECTS};

/// True for an IPv4 address an owner-chosen URL may not name.
fn ipv4_refused(ip: Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        // 100.64.0.0/10, shared address space (carrier-grade NAT).
        || (ip.octets()[0] == 100 && (ip.octets()[1] & 0xc0) == 0x40)
}

/// True for an IPv6 address an owner-chosen URL may not name.
fn ipv6_refused(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return ipv4_refused(v4);
    }
    let first = ip.segments()[0];
    ip.is_loopback()
        || ip.is_unspecified()
        // fc00::/7, unique local.
        || (first & 0xfe00) == 0xfc00
        // fe80::/10, link-local.
        || (first & 0xffc0) == 0xfe80
}

/// Checks a URL an owner cluster asks this host to fetch an archive from.
///
/// # Arguments
///
/// * `url` - The owner's artifact URL.
///
/// # Returns
///
/// The parsed URL.
///
/// # Errors
///
/// Returns an error when the URL does not parse, is not `https:`, carries
/// credentials, has no host, or names `localhost` (or a `.localhost`
/// name) or a loopback, private, shared, link-local, unique-local,
/// broadcast or unspecified IP literal.
pub fn check_owner_artifact_url(url: &str) -> Result<Url> {
    let parsed = Url::parse(url)
        .map_err(|_| CatalogError::message("the owner's artifact URL does not parse"))?;
    if parsed.scheme() != "https" {
        return Err(CatalogError::message(
            "an owner's artifact URL must be https:",
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(CatalogError::message(
            "an owner's artifact URL may not carry credentials",
        ));
    }
    let refused = match parsed.host() {
        None => true,
        Some(Host::Domain(name)) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            name == "localhost" || name.ends_with(".localhost")
        }
        Some(Host::Ipv4(ip)) => ipv4_refused(ip),
        Some(Host::Ipv6(ip)) => ipv6_refused(ip),
    };
    if refused {
        return Err(CatalogError::message(format!(
            "an owner's artifact URL may not name host `{}`",
            parsed.host_str().unwrap_or_default()
        )));
    }
    Ok(parsed)
}

/// Where a redirect from `current` to `location` leads, when it may be
/// followed: the resolved URL must pass [`check_owner_artifact_url`]. It
/// may name another origin.
///
/// # Arguments
///
/// * `current` - The URL that answered with a redirect.
/// * `location` - Its `Location` header, absolute or relative.
///
/// # Errors
///
/// Returns an error when the location does not resolve or the hop is
/// refused.
fn redirect_hop(current: &Url, location: &str) -> Result<Url> {
    let next = current
        .join(location)
        .map_err(|_| CatalogError::message("a redirect location does not parse"))?;
    check_owner_artifact_url(next.as_str())
}

/// Fetches an owner-chosen `https:` archive into `dest`, bounded by
/// [`MAX_DOWNLOAD_BYTES`] and [`DOWNLOAD_TIMEOUT`] per request. Redirects
/// are followed (at most [`MAX_REDIRECTS`]) only when each hop passes
/// [`check_owner_artifact_url`]; the caller checks the archive digest.
///
/// # Arguments
///
/// * `url` - The owner's artifact URL.
/// * `dest` - Where the body is written.
///
/// # Errors
///
/// Returns an error when a URL or a redirect hop is refused, there are
/// too many redirects, or the transfer fails or exceeds the cap.
pub fn fetch_owner_artifact(url: &str, dest: &Path) -> Result<()> {
    let mut current = check_owner_artifact_url(url)?;
    for _ in 0..=MAX_REDIRECTS {
        let mut response = ureq::get(current.as_str())
            .header(
                "User-Agent",
                concat!("bookclerk/", env!("CARGO_PKG_VERSION"), " (plugin-install)"),
            )
            .config()
            .timeout_global(Some(DOWNLOAD_TIMEOUT))
            .max_redirects(0)
            .build()
            .call()
            .map_err(|e| CatalogError::message(format!("download failed: {e}")))?;
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| CatalogError::message("a redirect without a location"))?;
            current = redirect_hop(&current, location)?;
            continue;
        }
        if !status.is_success() {
            return Err(CatalogError::message(format!("download HTTP {status}")));
        }
        let mut buf = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(MAX_DOWNLOAD_BYTES + 1)
            .read_to_end(&mut buf)?;
        if buf.len() as u64 > MAX_DOWNLOAD_BYTES {
            return Err(CatalogError::message("download exceeds size limit"));
        }
        write_file(dest, &buf)?;
        return Ok(());
    }
    Err(CatalogError::message("too many redirects"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_urls_name_only_public_https_hosts() {
        for allowed in [
            "https://registry.example/store-1.0.0.tar.gz",
            "https://objects.example:8443/a?b=c",
            "https://8.8.8.8/a.tar.gz",
            "https://[2001:db8::1]/a.tar.gz",
        ] {
            assert!(check_owner_artifact_url(allowed).is_ok(), "{allowed}");
        }
        for refused in [
            "http://registry.example/a.tar.gz",
            "file:///etc/passwd",
            "blob:sha256:ab",
            "https://localhost/a",
            "https://LOCALHOST./a",
            "https://api.localhost/a",
            "https://127.0.0.1/a",
            "https://2130706433/a",
            "https://0.0.0.0/a",
            "https://10.1.2.3/a",
            "https://172.16.0.1/a",
            "https://192.168.1.1/a",
            "https://169.254.169.254/latest/meta-data",
            "https://100.64.0.1/a",
            "https://[::1]/a",
            "https://[::]/a",
            "https://[fe80::1]/a",
            "https://[fd00::1]/a",
            "https://[::ffff:127.0.0.1]/a",
            "https://user:pass@registry.example/a",
            "not a url",
        ] {
            assert!(check_owner_artifact_url(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn redirects_are_followed_only_to_allowed_hosts() {
        let from = check_owner_artifact_url("https://registry.example/store.tar.gz").unwrap();
        // Another public https host (registry to object storage) is fine.
        assert_eq!(
            redirect_hop(&from, "https://objects.example/abc?sig=1")
                .unwrap()
                .as_str(),
            "https://objects.example/abc?sig=1"
        );
        assert_eq!(
            redirect_hop(&from, "/v2/store.tar.gz").unwrap().as_str(),
            "https://registry.example/v2/store.tar.gz"
        );
        for refused in [
            "https://127.0.0.1/a",
            "https://169.254.169.254/latest/meta-data",
            "https://localhost/a",
            "https://[::1]/a",
            "http://objects.example/a",
            "https://user:pass@objects.example/a",
            "file:///etc/passwd",
        ] {
            assert!(redirect_hop(&from, refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn a_refused_owner_url_is_not_fetched() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("archive");
        assert!(fetch_owner_artifact("https://127.0.0.1:1/a.tar.gz", &dest).is_err());
        assert!(!dest.exists());
    }
}
