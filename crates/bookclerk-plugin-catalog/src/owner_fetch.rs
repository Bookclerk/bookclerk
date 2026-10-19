//! Fetching an archive whose URL an owner cluster chose, on a host node.
//!
//! A host fetches owner `https:` archives itself, outside any plugin jail
//! and network consent, so the URL is held to more than the installer's
//! [`crate::fetch_artifact`]: `https:` only, no credentials in the URL, a
//! host that is not `localhost` or a non-public IP literal
//! ([`check_owner_artifact_url`]), and every redirect hop checked the same
//! way. A hop may change origin (registry downloads redirect to object
//! storage): the operator approved the URL and the archive digest, which
//! binds the bytes either way.
//!
//! The text of a URL says nothing about where a name resolves, so the same
//! address policy ([`owner_address_allowed`]) is enforced at connect time
//! too: the fetch's resolver ([`PublicOnlyResolver`]) refuses a name, IP
//! literals included, when any address it resolves to is not public, and
//! the connector dials only the addresses that resolver returned. A name
//! that resolves (or rebinds between hops) to a loopback, private,
//! link-local, unique-local, shared or metadata address is never reached,
//! on the first hop or any redirect. The fetch never uses a proxy from the
//! environment: a proxy resolves the target itself, past that resolver.

use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

use bookclerk_plugin_manifest::{canonical_ip, is_public_internet, is_restricted_hostname};
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{Connector, DefaultConnector, NextTimeout};
use ureq::Agent;
use url::{Host, Url};

use crate::error::{CatalogError, Result};
use crate::extract::write_file;
use crate::install::{DOWNLOAD_TIMEOUT, MAX_DOWNLOAD_BYTES, MAX_REDIRECTS};

/// True for an IPv4 address that [`is_public_internet`] lets through but an
/// owner fetch still may not reach: `240.0.0.0/4` (reserved) and
/// `192.0.0.0/24` (IETF protocol assignments).
fn ipv4_reserved(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    a >= 240 || (a == 192 && b == 0 && c == 0)
}

/// True for an IPv6 address that [`is_public_internet`] lets through but an
/// owner fetch still may not reach: forms that tunnel to or embed IPv4
/// space the IPv4 policy cannot see, and deprecated or discard ranges.
/// NAT64 (`64:ff9b::/96`) is judged by the IPv4 address it embeds, so an
/// IPv6-only host behind DNS64 can still reach a public IPv4 registry.
fn ipv6_refused(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        let [.., a, b, c, d] = ip.octets();
        return !owner_address_allowed(IpAddr::V4(Ipv4Addr::new(a, b, c, d)));
    }
    // ::/96, IPv4-compatible (deprecated; `::` and `::1` are refused anyway).
    s[..6] == [0, 0, 0, 0, 0, 0]
        // ::ffff:0:0/96, IPv4-translated (SIIT).
        || s[..6] == [0, 0, 0, 0, 0xffff, 0]
        // 64:ff9b:1::/48, local-use NAT64 with an operator-chosen layout.
        || (s[0] == 0x64 && s[1] == 0xff9b && s[2] == 1)
        // 2002::/16, 6to4 (deprecated).
        || s[0] == 0x2002
        // 2001::/32, Teredo.
        || (s[0] == 0x2001 && s[1] == 0)
        // fec0::/10, site-local (deprecated).
        || (s[0] & 0xffc0) == 0xfec0
        // 100::/64, discard-only.
        || (s[0] == 0x100 && s[1..4] == [0, 0, 0])
}

/// True when an owner fetch may connect to `ip`: a public Internet address
/// ([`is_public_internet`]: not loopback, private, shared, link-local,
/// unique-local, multicast, documentation, benchmark, broadcast or
/// unspecified; IPv4-mapped IPv6 judged as the IPv4 address it maps) and
/// none of the reserved or tunnelling forms it does not cover
/// ([`ipv4_reserved`], [`ipv6_refused`]).
fn owner_address_allowed(ip: IpAddr) -> bool {
    let ip = canonical_ip(ip);
    is_public_internet(ip)
        && match ip {
            IpAddr::V4(v4) => !ipv4_reserved(v4),
            IpAddr::V6(v6) => !ipv6_refused(v6),
        }
}

/// Checks a URL an owner cluster asks this host to fetch an archive from.
///
/// This is the textual half of the policy; the fetch enforces the address
/// half again on what each name resolves to ([`PublicOnlyResolver`]).
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
/// name, or another name that always resolves locally) or an IP literal
/// that is not a public Internet address.
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
        Some(Host::Domain(name)) => is_restricted_hostname(name),
        Some(Host::Ipv4(ip)) => !owner_address_allowed(IpAddr::V4(ip)),
        Some(Host::Ipv6(ip)) => !owner_address_allowed(IpAddr::V6(ip)),
    };
    if refused {
        return Err(CatalogError::message(format!(
            "an owner's artifact URL may not name host `{}`",
            parsed.host_str().unwrap_or_default()
        )));
    }
    Ok(parsed)
}

/// Resolves the names an owner fetch connects to and refuses a name when
/// any address it resolves to fails [`owner_address_allowed`].
///
/// ureq hands the connector exactly the addresses this returns (with no
/// proxy configured it never resolves elsewhere), so the check is on the
/// addresses actually dialed, not on a separate lookup a rebinding name
/// could answer differently. IP-literal hosts go through it too. A name
/// with any refused address is refused whole rather than filtered, so a
/// name that mixes public and private answers is never half-trusted.
#[derive(Debug)]
struct PublicOnlyResolver<R> {
    /// The resolver that does the lookup.
    inner: R,
}

impl<R: Resolver> Resolver for PublicOnlyResolver<R> {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> std::result::Result<ResolvedSocketAddrs, ureq::Error> {
        let addrs = self.inner.resolve(uri, config, timeout)?;
        if addrs.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        if addrs.iter().any(|addr| !owner_address_allowed(addr.ip())) {
            return Err(ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "host `{}` resolves to an address an owner's artifact may not be fetched from",
                    uri.host().unwrap_or_default()
                ),
            )));
        }
        Ok(addrs)
    }
}

/// The agent an owner fetch runs on: no proxy (a proxy, even one from
/// `HTTPS_PROXY` / `ALL_PROXY`, would resolve the target past `resolver`),
/// no automatic redirects (each hop is checked by [`redirect_hop`]), and
/// [`DOWNLOAD_TIMEOUT`] per request.
///
/// # Arguments
///
/// * `connector` - Opens the transport ([`DefaultConnector`] outside tests).
/// * `resolver` - Resolves each hop's host (a [`PublicOnlyResolver`]).
fn owner_agent(connector: impl Connector, resolver: PublicOnlyResolver<impl Resolver>) -> Agent {
    let config = Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .build();
    Agent::with_parts(config, connector, resolver)
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
/// [`check_owner_artifact_url`], and every hop connects only to addresses
/// that pass [`owner_address_allowed`] ([`PublicOnlyResolver`]); no proxy
/// is used. The caller checks the archive digest.
///
/// # Arguments
///
/// * `url` - The owner's artifact URL.
/// * `dest` - Where the body is written.
///
/// # Errors
///
/// Returns an error when a URL or a redirect hop is refused, a host
/// resolves to a refused address, there are too many redirects, or the
/// transfer fails or exceeds the cap.
pub fn fetch_owner_artifact(url: &str, dest: &Path) -> Result<()> {
    let agent = owner_agent(
        DefaultConnector::default(),
        PublicOnlyResolver {
            inner: DefaultResolver::default(),
        },
    );
    fetch_with(&agent, url, dest)
}

/// [`fetch_owner_artifact`] on `agent` (an [`owner_agent`]).
///
/// # Errors
///
/// As [`fetch_owner_artifact`].
fn fetch_with(agent: &Agent, url: &str, dest: &Path) -> Result<()> {
    let mut current = check_owner_artifact_url(url)?;
    for _ in 0..=MAX_REDIRECTS {
        let mut response = agent
            .get(current.as_str())
            .header(
                "User-Agent",
                concat!("bookclerk/", env!("CARGO_PKG_VERSION"), " (plugin-install)"),
            )
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
    use std::collections::BTreeMap;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use ureq::unversioned::transport::{Buffers, ConnectionDetails, LazyBuffers, Transport};

    use super::*;

    #[test]
    fn owner_urls_name_only_public_https_hosts() {
        for allowed in [
            "https://registry.example/store-1.0.0.tar.gz",
            "https://objects.example:8443/a?b=c",
            "https://8.8.8.8/a.tar.gz",
            "https://[2606:4700:4700::1111]/a.tar.gz",
            "https://[64:ff9b::808:808]/a.tar.gz",
            // Only what it resolves to can tell; the resolver decides.
            "https://localtest.me/a.tar.gz",
            "https://169.254.169.254.nip.io/a.tar.gz",
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
            "https://metadata.google.internal/a",
            "https://127.0.0.1/a",
            "https://2130706433/a",
            "https://0x7f.1/a",
            "https://0177.0.0.1/a",
            "https://0.0.0.0/a",
            "https://10.1.2.3/a",
            "https://172.16.0.1/a",
            "https://192.168.1.1/a",
            "https://169.254.169.254/latest/meta-data",
            "https://100.64.0.1/a",
            "https://240.0.0.1/a",
            "https://[::1]/a",
            "https://[::]/a",
            "https://[fe80::1]/a",
            "https://[fd00::1]/a",
            "https://[::ffff:127.0.0.1]/a",
            "https://[::ffff:a9fe:a9fe]/a",
            "https://[::127.0.0.1]/a",
            "https://[64:ff9b::7f00:1]/a",
            "https://[64:ff9b::a9fe:a9fe]/a",
            "https://[2002:7f00:1::1]/a",
            "https://[2001:0:4136:e378::1]/a",
            "https://[2001:db8::1]/a",
            "https://[fec0::1]/a",
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

    /// Answers each host from a fixed table, like a DNS server the owner
    /// controls.
    #[derive(Debug, Clone, Default)]
    struct StubResolver {
        /// Host to the addresses it resolves to.
        answers: BTreeMap<String, Vec<SocketAddr>>,
    }

    impl StubResolver {
        /// `host` resolves to `ips` on port 443.
        fn with(mut self, host: &str, ips: &[&str]) -> Self {
            let addrs = ips
                .iter()
                .map(|ip| SocketAddr::new(ip.parse().unwrap(), 443))
                .collect();
            self.answers.insert(host.to_string(), addrs);
            self
        }
    }

    impl Resolver for StubResolver {
        fn resolve(
            &self,
            uri: &Uri,
            _config: &Config,
            _timeout: NextTimeout,
        ) -> std::result::Result<ResolvedSocketAddrs, ureq::Error> {
            let mut out = self.empty();
            for addr in self
                .answers
                .get(uri.host().unwrap_or_default())
                .ok_or(ureq::Error::HostNotFound)?
            {
                out.push(*addr);
            }
            Ok(out)
        }
    }

    /// A [`NextTimeout`] that never fires.
    fn no_timeout() -> NextTimeout {
        NextTimeout {
            after: ureq::unversioned::transport::time::Duration::NotHappening,
            reason: ureq::Timeout::Global,
        }
    }

    /// Resolves `url` through a [`PublicOnlyResolver`] over `inner`.
    fn resolve_through(
        inner: impl Resolver,
        url: &str,
    ) -> std::result::Result<Vec<SocketAddr>, ureq::Error> {
        let uri: Uri = url.parse().unwrap();
        PublicOnlyResolver { inner }
            .resolve(&uri, &Config::default(), no_timeout())
            .map(|addrs| addrs.to_vec())
    }

    #[test]
    fn the_resolver_refuses_names_that_resolve_to_non_public_addresses() {
        for ip in [
            "127.0.0.1",
            "127.1.2.3",
            "10.0.0.1",
            "172.16.5.4",
            "192.168.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "240.0.0.1",
            "255.255.255.255",
            "224.0.0.1",
            "::1",
            "::",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "::ffff:0:7f00:1",
            "fe80::1",
            "fd00:ec2::254",
            "fec0::1",
            "ff02::1",
            "64:ff9b::7f00:1",
            "64:ff9b::a00:1",
            "64:ff9b:1::1",
            "2002:7f00:1::1",
            "2001:0:4136:e378::1",
        ] {
            let stub = StubResolver::default().with("rebind.example", &[ip]);
            assert!(
                resolve_through(stub, "https://rebind.example/a").is_err(),
                "{ip}"
            );
        }
        // One refused answer among public ones refuses the name.
        let mixed = StubResolver::default().with("mixed.example", &["8.8.8.8", "10.0.0.1"]);
        assert!(resolve_through(mixed, "https://mixed.example/a").is_err());

        let public = StubResolver::default().with(
            "registry.example",
            &["8.8.8.8", "2606:4700:4700::1111", "64:ff9b::808:808"],
        );
        assert_eq!(
            resolve_through(public, "https://registry.example/a")
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn ip_literals_and_local_names_are_checked_by_the_resolver_too() {
        // The system resolver answers these without any DNS traffic; the
        // policy applies to its answer whatever the URL text looked like.
        for url in [
            "https://127.0.0.1/a",
            "https://[::1]/a",
            "https://[::ffff:127.0.0.1]/a",
            "https://10.0.0.1/a",
            "https://169.254.169.254/a",
            "https://localhost/a",
            "https://0177.0.0.1/a",
            "https://2130706433/a",
        ] {
            assert!(
                resolve_through(DefaultResolver::default(), url).is_err(),
                "{url}"
            );
        }
    }

    #[test]
    fn the_owner_agent_never_uses_a_proxy() {
        // `Config::default()` reads HTTPS_PROXY / ALL_PROXY; the owner
        // agent must not, or the proxy would resolve the target itself.
        let agent = owner_agent(
            DefaultConnector::default(),
            PublicOnlyResolver {
                inner: DefaultResolver::default(),
            },
        );
        assert!(agent.config().proxy().is_none());
        assert_eq!(agent.config().max_redirects(), 0);
    }

    /// What [`FakeConnector`] was asked to dial: each host and the
    /// addresses it was handed.
    type Dialed = Arc<Mutex<Vec<(String, Vec<SocketAddr>)>>>;

    /// Serves canned HTTPS responses by host without a network, recording
    /// what it was asked to dial.
    #[derive(Debug, Clone, Default)]
    struct FakeConnector {
        /// Host to the raw HTTP/1.1 response it answers with.
        responses: BTreeMap<String, &'static str>,
        /// Every connection opened.
        dialed: Dialed,
    }

    impl Connector for FakeConnector {
        type Out = FakeTransport;

        fn connect(
            &self,
            details: &ConnectionDetails,
            _chained: Option<()>,
        ) -> std::result::Result<Option<Self::Out>, ureq::Error> {
            let host = details.uri.host().unwrap_or_default().to_string();
            self.dialed
                .lock()
                .unwrap()
                .push((host.clone(), details.addrs.to_vec()));
            let response = self
                .responses
                .get(&host)
                .ok_or(ureq::Error::ConnectionFailed)?;
            Ok(Some(FakeTransport {
                buffers: LazyBuffers::new(16 * 1024, 16 * 1024),
                response: response.as_bytes(),
            }))
        }
    }

    /// One canned response, sent once the request has been written.
    #[derive(Debug)]
    struct FakeTransport {
        /// ureq's buffers for this transport.
        buffers: LazyBuffers,
        /// Response bytes not yet handed to ureq.
        response: &'static [u8],
    }

    impl Transport for FakeTransport {
        fn buffers(&mut self) -> &mut dyn Buffers {
            &mut self.buffers
        }

        fn transmit_output(
            &mut self,
            _amount: usize,
            _timeout: NextTimeout,
        ) -> std::result::Result<(), ureq::Error> {
            Ok(())
        }

        fn await_input(&mut self, _timeout: NextTimeout) -> std::result::Result<bool, ureq::Error> {
            let space = self.buffers.input_append_buf();
            let n = space.len().min(self.response.len());
            space[..n].copy_from_slice(&self.response[..n]);
            self.buffers.input_appended(n);
            self.response = &self.response[n..];
            Ok(n > 0)
        }

        fn is_open(&mut self) -> bool {
            true
        }

        fn is_tls(&self) -> bool {
            true
        }
    }

    /// An owner agent over `connector` whose names resolve through `stub`.
    fn fake_agent(connector: &FakeConnector, stub: StubResolver) -> Agent {
        owner_agent(connector.clone(), PublicOnlyResolver { inner: stub })
    }

    const ARCHIVE: &str =
        "HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\narchive";

    #[test]
    fn a_public_host_is_fetched_from_the_addresses_the_resolver_returned() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("archive");
        let connector = FakeConnector {
            responses: BTreeMap::from([("registry.example".to_string(), ARCHIVE)]),
            ..FakeConnector::default()
        };
        let stub = StubResolver::default().with("registry.example", &["8.8.8.8"]);
        fetch_with(
            &fake_agent(&connector, stub),
            "https://registry.example/a.tar.gz",
            &dest,
        )
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"archive");
        assert_eq!(
            *connector.dialed.lock().unwrap(),
            vec![(
                "registry.example".to_string(),
                vec!["8.8.8.8:443".parse().unwrap()]
            )]
        );
    }

    #[test]
    fn a_name_that_resolves_to_a_private_address_is_never_dialed() {
        for ip in [
            "127.0.0.1",
            "10.0.0.7",
            "169.254.169.254",
            "::1",
            "::ffff:127.0.0.1",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let dest = dir.path().join("archive");
            let connector = FakeConnector {
                responses: BTreeMap::from([("localtest.me".to_string(), ARCHIVE)]),
                ..FakeConnector::default()
            };
            let stub = StubResolver::default().with("localtest.me", &[ip]);
            let err = fetch_with(
                &fake_agent(&connector, stub),
                "https://localtest.me/a.tar.gz",
                &dest,
            )
            .unwrap_err();
            assert!(err.to_string().contains("resolves to"), "{ip}: {err}");
            assert!(connector.dialed.lock().unwrap().is_empty(), "{ip}");
            assert!(!dest.exists(), "{ip}");
        }
    }

    #[test]
    fn a_redirect_to_a_name_that_resolves_to_a_private_address_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("archive");
        // The redirect target's URL passes the textual check; only its
        // resolution is local.
        let connector = FakeConnector {
            responses: BTreeMap::from([
                (
                    "registry.example".to_string(),
                    "HTTP/1.1 302 Found\r\nLocation: https://169.254.169.254.nip.io/latest/meta-data\r\nContent-Length: 0\r\n\r\n",
                ),
                ("169.254.169.254.nip.io".to_string(), ARCHIVE),
            ]),
            ..FakeConnector::default()
        };
        let stub = StubResolver::default()
            .with("registry.example", &["8.8.8.8"])
            .with("169.254.169.254.nip.io", &["169.254.169.254"]);
        let err = fetch_with(
            &fake_agent(&connector, stub),
            "https://registry.example/a.tar.gz",
            &dest,
        )
        .unwrap_err();
        assert!(err.to_string().contains("resolves to"), "{err}");
        let dialed = connector.dialed.lock().unwrap();
        assert_eq!(dialed.len(), 1);
        assert_eq!(dialed[0].0, "registry.example");
        assert!(!dest.exists());
    }
}
