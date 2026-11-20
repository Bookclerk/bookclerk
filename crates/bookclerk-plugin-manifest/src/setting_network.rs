//! Network destinations named by plugin settings.
//!
//! A `[[settings]]` entry may declare `network = "fetch"` (its value is a URL)
//! or `network = "tcp"` (its value is `host:port`). The host turns the value
//! an operator saved into one [`SettingDestination`] with
//! [`derive_setting_destination`], lists it on the consent form with the
//! setting as its source, and adds it to the plugin's egress policy only once
//! the operator approved that exact destination.
//!
//! The value is untrusted text (an operator typed it, or a plugin's own
//! default supplied it), so derivation is a pure function over the string
//! and is fuzzed (`fuzz/fuzz_targets/setting_destination.rs`).

use std::fmt;
use std::net::IpAddr;

use bookclerk_plugin_abi::{SettingNetwork, MAX_SETTING_STRING_BYTES};
use serde::{Deserialize, Serialize};

use crate::address::{canonical_ip, is_local_network, is_public_internet, is_restricted_hostname};
use crate::egress::{normalize_hostname, TcpGrant};

/// What kind of address a setting's host is, which decides how the grant
/// reaches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DestinationScope {
    /// A host name. It may resolve to a public address, or to a local-network
    /// address (RFC 1918, shared CGNAT space, IPv6 ULA) for this name only.
    Name,
    /// A public IP literal.
    PublicAddress,
    /// A local-network IP literal; the grant reaches exactly that address.
    LocalAddress,
    /// Loopback, link-local (cloud metadata), another special address, or a
    /// restricted name such as `localhost`. The destination is listed, but the
    /// connection still needs an address grant the operator adds explicitly.
    Restricted,
}

/// The one network destination a setting's value names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingDestination {
    /// Whether the plugin fetches it or opens a raw socket to it.
    pub network: SettingNetwork,
    /// The host: an IDNA-normalized name, or a canonical IP literal (IPv6
    /// without brackets, IPv4-mapped IPv6 unwrapped).
    pub host: String,
    /// The port, 1-65535: the value's own, else the setting's
    /// `default_port`, else the URL scheme's.
    pub port: u16,
    /// True for an `https` fetch, whose certificate is verified; false for
    /// plain HTTP and raw TCP, which trust the resolver.
    pub tls: bool,
    /// What kind of address [`Self::host`] is.
    pub scope: DestinationScope,
}

impl SettingDestination {
    /// The host as it appears beside a port: IPv6 in brackets.
    fn host_for_port(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }

    /// The `domains` entry a fetch destination adds (`host:port`), or `None`
    /// for a TCP destination.
    #[must_use]
    pub fn domain_entry(&self) -> Option<String> {
        match self.network {
            SettingNetwork::Fetch => Some(format!("{}:{}", self.host_for_port(), self.port)),
            SettingNetwork::Tcp => None,
        }
    }

    /// The TCP grant a TCP destination adds, or `None` for a fetch.
    #[must_use]
    pub fn tcp_grant(&self) -> Option<TcpGrant> {
        match self.network {
            SettingNetwork::Tcp => Some(TcpGrant {
                host: self.host.clone(),
                ports: vec![self.port],
            }),
            SettingNetwork::Fetch => None,
        }
    }

    /// The [`crate::EgressPolicy::local_names`] entry this destination adds:
    /// its name, or its local-network address. `None` for a public or
    /// restricted address.
    #[must_use]
    pub fn local_name(&self) -> Option<String> {
        match self.scope {
            DestinationScope::Name | DestinationScope::LocalAddress => Some(self.host.clone()),
            DestinationScope::PublicAddress | DestinationScope::Restricted => None,
        }
    }

    /// How the destination reads on the consent form and in logs:
    /// `https://abs.home.lan:13378` for a fetch, `db.home.lan:5432` for TCP.
    #[must_use]
    pub fn display(&self) -> String {
        match self.network {
            SettingNetwork::Fetch => {
                let scheme = if self.tls { "https" } else { "http" };
                format!("{scheme}://{}:{}", self.host_for_port(), self.port)
            }
            SettingNetwork::Tcp => format!("{}:{}", self.host_for_port(), self.port),
        }
    }
}

/// Why a setting value names no usable destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingDestinationError {
    /// The value is empty: the setting names no destination.
    Empty,
    /// Longer than a setting value may be, or holds whitespace or control
    /// characters.
    Malformed,
    /// A `fetch` URL whose scheme is not `http` or `https`.
    Scheme,
    /// The value carries a user name or password; credentials belong in a
    /// secret setting.
    Credentials,
    /// The host is missing, a wildcard, percent-encoded or not a valid name.
    Host,
    /// The port is missing (a bare `tcp` host without `default_port`), zero
    /// or not a number.
    Port,
}

impl fmt::Display for SettingDestinationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "the setting is empty",
            Self::Malformed => "the value is too long or holds spaces or control characters",
            Self::Scheme => "the URL must start with http:// or https://",
            Self::Credentials => {
                "the value holds a user name or password; put credentials in a secret setting"
            }
            Self::Host => "the value does not name a valid host",
            Self::Port => "the value names no valid port",
        })
    }
}

impl std::error::Error for SettingDestinationError {}

/// Derives the destination a network setting's value names.
///
/// - `fetch`: an `http://` or `https://` URL; a value without `://` is read
///   as `https://<value>`. The port is the URL's own, else `default_port`
///   for a bare value, else the scheme's (80 or 443). Paths and queries are
///   ignored.
/// - `tcp`: `host:port`, `[v6]:port`, a bare host (port from
///   `default_port`), or a URL of any scheme (its host and port, else
///   `default_port`), so a `postgres://db:5432/x` value works.
///
/// A user name or password anywhere is refused, and so is a wildcard host.
///
/// # Arguments
///
/// * `network` - The setting's declared [`SettingNetwork`].
/// * `default_port` - The setting's declared `default_port`.
/// * `value` - The saved value (surrounding whitespace is ignored).
///
/// # Returns
///
/// The normalized destination.
///
/// # Errors
///
/// [`SettingDestinationError`] naming the first problem.
///
/// # Examples
///
/// ```
/// use bookclerk_plugin_abi::SettingNetwork;
/// use bookclerk_plugin_manifest::{derive_setting_destination, DestinationScope};
///
/// let abs = derive_setting_destination(SettingNetwork::Fetch, None, "http://ABS.home.lan:13378/")
///     .unwrap();
/// assert_eq!(abs.domain_entry().as_deref(), Some("abs.home.lan:13378"));
/// assert_eq!(abs.scope, DestinationScope::Name);
///
/// let db = derive_setting_destination(SettingNetwork::Tcp, Some(5432), "192.168.1.5").unwrap();
/// assert_eq!(db.display(), "192.168.1.5:5432");
/// assert_eq!(db.scope, DestinationScope::LocalAddress);
/// ```
pub fn derive_setting_destination(
    network: SettingNetwork,
    default_port: Option<u16>,
    value: &str,
) -> Result<SettingDestination, SettingDestinationError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(SettingDestinationError::Empty);
    }
    if value.len() > MAX_SETTING_STRING_BYTES
        || value
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(SettingDestinationError::Malformed);
    }
    if default_port == Some(0) {
        return Err(SettingDestinationError::Port);
    }
    let has_scheme = match value.split_once("://") {
        // Only a plain scheme name may precede `://`, so `http:s://h` is not
        // read as some other URL than the one written.
        Some((scheme, _)) => {
            let mut chars = scheme.chars();
            let plain = chars.next().is_some_and(|ch| ch.is_ascii_alphabetic())
                && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'));
            if !plain {
                return Err(SettingDestinationError::Malformed);
            }
            true
        }
        None => false,
    };
    // A URL parser decodes a percent-encoded host, so what was written would
    // not be what is checked: refuse `%` anywhere in the authority.
    let authority = value.split_once("://").map_or(value, |(_, rest)| rest);
    let authority = authority.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('%') {
        return Err(SettingDestinationError::Host);
    }
    // Any `@` in the authority is user info, even an empty one the URL
    // parser would drop silently.
    if authority.contains('@') {
        return Err(SettingDestinationError::Credentials);
    }
    let (host, port, tls) = match network {
        SettingNetwork::Fetch => {
            let text = if has_scheme {
                value.to_string()
            } else {
                format!("https://{value}")
            };
            let url = url::Url::parse(&text).map_err(|_| SettingDestinationError::Host)?;
            let tls = match url.scheme() {
                "https" => true,
                "http" => false,
                _ => return Err(SettingDestinationError::Scheme),
            };
            refuse_credentials(&url)?;
            let host = url_host(&url)?;
            let port = match (url.port(), has_scheme) {
                (Some(port), _) => port,
                (None, false) => default_port.unwrap_or(443),
                (None, true) => url
                    .port_or_known_default()
                    .ok_or(SettingDestinationError::Port)?,
            };
            (host, port, tls)
        }
        SettingNetwork::Tcp if has_scheme => {
            let url = url::Url::parse(value).map_err(|_| SettingDestinationError::Host)?;
            refuse_credentials(&url)?;
            let host = url_host(&url)?;
            let port = url
                .port()
                .or(default_port)
                .ok_or(SettingDestinationError::Port)?;
            (host, port, false)
        }
        SettingNetwork::Tcp => {
            let (host, port) = split_host_port(value)?;
            let port = port.or(default_port).ok_or(SettingDestinationError::Port)?;
            (normalize_host(host)?, port, false)
        }
    };
    if port == 0 {
        return Err(SettingDestinationError::Port);
    }
    let scope = scope_of(&host);
    Ok(SettingDestination {
        network,
        host,
        port,
        tls,
        scope,
    })
}

/// Refuses a URL that carries a user name or password.
///
/// # Errors
///
/// [`SettingDestinationError::Credentials`] when the URL has either.
fn refuse_credentials(url: &url::Url) -> Result<(), SettingDestinationError> {
    if url.username().is_empty() && url.password().is_none() {
        Ok(())
    } else {
        Err(SettingDestinationError::Credentials)
    }
}

/// The normalized host of a parsed URL.
///
/// # Errors
///
/// [`SettingDestinationError::Host`] when the URL has no host or the name
/// does not normalize.
fn url_host(url: &url::Url) -> Result<String, SettingDestinationError> {
    match url.host() {
        Some(url::Host::Domain(name)) => normalize_host(name),
        Some(url::Host::Ipv4(v4)) => Ok(IpAddr::V4(v4).to_string()),
        Some(url::Host::Ipv6(v6)) => Ok(canonical_ip(IpAddr::V6(v6)).to_string()),
        None => Err(SettingDestinationError::Host),
    }
}

/// Splits `host:port`, `[v6]:port`, `[v6]`, a bare IPv6 literal or a bare
/// host. A port must be plain decimal.
///
/// # Errors
///
/// [`SettingDestinationError::Host`] for an unclosed bracket and
/// [`SettingDestinationError::Port`] for a malformed port.
fn split_host_port(value: &str) -> Result<(&str, Option<u16>), SettingDestinationError> {
    if let Some(rest) = value.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or(SettingDestinationError::Host)?;
        return match after {
            "" => Ok((host, None)),
            _ => {
                let port = after
                    .strip_prefix(':')
                    .ok_or(SettingDestinationError::Port)?;
                Ok((host, Some(parse_port(port)?)))
            }
        };
    }
    match value.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => Ok((host, Some(parse_port(port)?))),
        _ => Ok((value, None)),
    }
}

/// A port of plain decimal digits in 1-65535 (leading zeros refused).
///
/// # Errors
///
/// [`SettingDestinationError::Port`] for anything else.
fn parse_port(text: &str) -> Result<u16, SettingDestinationError> {
    if text.is_empty() || text.starts_with('0') || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(SettingDestinationError::Port);
    }
    text.parse().map_err(|_| SettingDestinationError::Port)
}

/// A canonical IP literal or IDNA-normalized name; never a wildcard.
///
/// # Errors
///
/// [`SettingDestinationError::Host`] for an empty, wildcard, zone-scoped or
/// otherwise invalid host.
fn normalize_host(host: &str) -> Result<String, SettingDestinationError> {
    let host = host.trim_end_matches('.');
    if host.is_empty() || host.contains('*') || host.contains('%') {
        return Err(SettingDestinationError::Host);
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(canonical_ip(ip).to_string());
    }
    let name = normalize_hostname(host).ok_or(SettingDestinationError::Host)?;
    // Letters, digits, `-`, `_` and dots only once IDNA has run: a real host
    // name, never URL punctuation the parser let through.
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(SettingDestinationError::Host);
    }
    // A legacy numeric spelling (`127.1`, `0x7f.1`) is an address, as a URL
    // parser reads it.
    match url::Host::parse(&name) {
        Ok(url::Host::Domain(_)) => Ok(name),
        Ok(url::Host::Ipv4(v4)) => Ok(IpAddr::V4(v4).to_string()),
        Ok(url::Host::Ipv6(v6)) => Ok(canonical_ip(IpAddr::V6(v6)).to_string()),
        Err(_) => Err(SettingDestinationError::Host),
    }
}

/// The scope of a normalized host.
fn scope_of(host: &str) -> DestinationScope {
    match host.parse::<IpAddr>() {
        Ok(ip) if is_public_internet(ip) => DestinationScope::PublicAddress,
        Ok(ip) if is_local_network(ip) => DestinationScope::LocalAddress,
        Ok(_) => DestinationScope::Restricted,
        Err(_) if is_restricted_hostname(host) => DestinationScope::Restricted,
        Err(_) => DestinationScope::Name,
    }
}

#[cfg(test)]
mod tests {
    // Test code: unwrap on known-good literals is the assertion; tests carry
    // no Errors or Panics sections.
    #![allow(
        clippy::unwrap_used,
        clippy::missing_panics_doc,
        clippy::missing_errors_doc
    )]

    use super::*;

    fn fetch(value: &str) -> Result<SettingDestination, SettingDestinationError> {
        derive_setting_destination(SettingNetwork::Fetch, None, value)
    }

    fn tcp(port: Option<u16>, value: &str) -> Result<SettingDestination, SettingDestinationError> {
        derive_setting_destination(SettingNetwork::Tcp, port, value)
    }

    #[test]
    fn fetch_values_read_as_urls() {
        let abs = fetch(" http://ABS.Home.Lan.:13378/api?x=1 ").unwrap();
        assert_eq!(abs.host, "abs.home.lan");
        assert_eq!((abs.port, abs.tls), (13378, false));
        assert_eq!(abs.domain_entry().as_deref(), Some("abs.home.lan:13378"));
        assert_eq!(abs.local_name().as_deref(), Some("abs.home.lan"));
        assert_eq!(abs.display(), "http://abs.home.lan:13378");
        let bare = fetch("s3.example.com").unwrap();
        assert_eq!((bare.port, bare.tls), (443, true));
        assert_eq!(bare.scope, DestinationScope::Name);
        let https = fetch("https://s3.example.com:443").unwrap();
        assert_eq!(https.port, 443);
        let custom =
            derive_setting_destination(SettingNetwork::Fetch, Some(9000), "minio.lan").unwrap();
        assert_eq!(custom.port, 9000);
        let v6 = fetch("https://[FD00::5]:8443/").unwrap();
        assert_eq!(v6.host, "fd00::5");
        assert_eq!(v6.domain_entry().as_deref(), Some("[fd00::5]:8443"));
        assert_eq!(v6.scope, DestinationScope::LocalAddress);
        assert_eq!(
            fetch("ftp://x.example").unwrap_err(),
            SettingDestinationError::Scheme
        );
        assert_eq!(
            fetch("https://user:pw@x.example").unwrap_err(),
            SettingDestinationError::Credentials
        );
    }

    #[test]
    fn tcp_values_read_as_host_and_port() {
        let db = tcp(Some(5432), "db.home.lan").unwrap();
        assert_eq!(db.tcp_grant().unwrap().ports, vec![5432]);
        assert_eq!(db.domain_entry(), None);
        assert_eq!(tcp(None, "db.home.lan:6543").unwrap().port, 6543);
        assert_eq!(tcp(None, "[fd00::1]:5432").unwrap().host, "fd00::1");
        assert_eq!(tcp(Some(1), "fd00::1").unwrap().port, 1);
        let url = tcp(Some(5432), "postgres://db.home.lan/library").unwrap();
        assert_eq!((url.host.as_str(), url.port), ("db.home.lan", 5432));
        assert_eq!(
            tcp(None, "db.home.lan").unwrap_err(),
            SettingDestinationError::Port
        );
        assert_eq!(
            tcp(None, "db:0").unwrap_err(),
            SettingDestinationError::Port
        );
        assert_eq!(
            tcp(None, "db:05432").unwrap_err(),
            SettingDestinationError::Port
        );
        assert_eq!(
            tcp(None, "postgres://u:p@db:5432").unwrap_err(),
            SettingDestinationError::Credentials
        );
    }

    #[test]
    fn scopes_follow_the_address() {
        assert_eq!(
            tcp(Some(1), "8.8.8.8").unwrap().scope,
            DestinationScope::PublicAddress
        );
        assert_eq!(
            tcp(Some(1), "10.0.0.1").unwrap().scope,
            DestinationScope::LocalAddress
        );
        assert_eq!(tcp(Some(1), "::ffff:10.0.0.1").unwrap().host, "10.0.0.1");
        for restricted in [
            "127.0.0.1",
            "127.1",
            "169.254.169.254",
            "fe80::1",
            "localhost",
            "::1",
        ] {
            let found = tcp(Some(1), restricted).unwrap();
            assert_eq!(found.scope, DestinationScope::Restricted, "{restricted}");
            assert_eq!(found.local_name(), None, "{restricted}");
        }
    }

    #[test]
    fn malformed_values_are_refused() {
        for value in [
            "",
            "  ",
            "a b.example",
            "x.exa\tmple",
            "*.example.com",
            "%41.example",
        ] {
            assert!(fetch(value).is_err(), "{value:?}");
            assert!(tcp(Some(1), value).is_err(), "{value:?}");
        }
        assert_eq!(fetch("").unwrap_err(), SettingDestinationError::Empty);
        let long = "a".repeat(MAX_SETTING_STRING_BYTES + 1);
        assert_eq!(
            fetch(&long).unwrap_err(),
            SettingDestinationError::Malformed
        );
    }
}
