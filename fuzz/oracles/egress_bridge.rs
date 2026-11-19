//! Shared oracle for the `egress_bridge` fuzz target and its corpus replay
//! (`crates/bookclerk-workerd/tests/fuzz_corpus.rs`): the workerd egress gate
//! (`crates/bookclerk-workerd/bridge/egress.js`, `allowsInitial`,
//! `allowsRedirect`, `allowsTcp`) against the Rust `EgressPolicy`
//! (`bookclerk-plugin-manifest`), which gates native guests and is the
//! reference (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is the JSON wire form of an `EgressPolicy`, exactly what the host
//! injects into workerd as `EGRESS_POLICY`. Only policies the host can emit are
//! compared: every `domains` entry and `tcp` host already normalized, every
//! `addressCidrs` entry in its `network/prefix` form. For each
//! policy the oracle derives probe hosts (the grants' own names and their
//! near misses, restricted names, and IP literals in every range and
//! spelling) and hands the JavaScript gate each one as workerd would: a fetch
//! host as the URL parser prints it, a `connect()` host as the bridge's own
//! `canonicalSocketHost` returns it. Both are modelled here with the `url`
//! crate (QuickJS has no `URL`); the Node replay checks the models against
//! Node's URL parser and the real `canonicalSocketHost`. The invariant: the JavaScript gate never allows
//! what the Rust policy refuses, and for a hostname (not an IP literal) the
//! two agree exactly.
//!
//! This file needs no JavaScript engine: the fuzz target runs `egress.js` in
//! an embedded one, and the replay test writes the Rust side of every probe
//! to `packages/plugin-sdk/scripts/egress_bridge_vectors.json`, which the
//! SDK's `test:egress-bridge` script checks `egress.js` against under Node.

use bookclerk_plugin_manifest::{
    normalize_domain_pattern, normalize_tcp_host, CidrGrant, EgressPolicy, NetworkMode,
    ANY_PUBLIC_HOST,
};

/// Largest policy document worth decoding.
pub const MAX_INPUT_BYTES: usize = 8 * 1024;

/// Most probe hosts derived from one policy, so one input stays fast.
pub const MAX_PROBES: usize = 160;

/// The bridge source the host embeds in every workerd plugin config.
pub const EGRESS_JS: &str = include_str!("../../crates/bookclerk-workerd/bridge/egress.js");

/// Hosts every policy is probed with: restricted names, IP literals in each
/// range the policy treats differently, legacy and bracketed spellings, and
/// names the URL parser refuses.
pub const FIXED_PROBES: &[&str] = &[
    "localhost",
    "LOCALHOST.",
    "a.localhost",
    "localhost.localdomain",
    "metadata.google.internal",
    "metadata.google.internal.",
    "127.0.0.1",
    "127.1",
    "2130706433",
    "0x7f.0.0.1",
    "0177.0.0.1",
    "127.0.0.1.",
    "::1",
    "[::1]",
    "[::ffff:7f00:1]",
    "::ffff:127.0.0.1",
    "0.0.0.0",
    "10.0.0.1",
    "172.16.0.1",
    "192.168.1.1",
    "169.254.169.254",
    "100.64.0.1",
    "198.18.0.1",
    "192.0.2.1",
    "198.51.100.7",
    "203.0.113.9",
    "224.0.0.1",
    "255.255.255.255",
    "8.8.8.8",
    "0x08.8.8.8",
    "[2606:4700::1111]",
    "2606:4700::1111",
    "[::ffff:808:808]",
    "[fd00::1]",
    "[fe80::1]",
    "[ff02::1]",
    "[2001:db8::1]",
    "[::]",
    "a.123",
    "1.2.3.4.5",
    "xn--zz.example.com",
    "a_b.example.com",
    "example.com",
    "%6c%6fcalhost",
    "user@example.com",
];

/// The JavaScript gate's verdicts, or the Rust policy's, for one probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Verdicts {
    /// The host may be fetched directly (hop 0).
    pub initial: bool,
    /// A redirect to the host is followed at [`Probe::hop`].
    pub redirect: bool,
    /// A raw TCP `connect()` to the host at [`Probe::port`] is allowed.
    pub tcp: bool,
}

/// One probe host, in each spelling a gate sees it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Probe {
    /// The host as derived, before any canonicalization.
    pub host: String,
    /// The fetch hostname the URL parser prints for it; `None` when a URL
    /// naming it does not parse, so no fetch reaches the gate.
    pub fetch: Option<String>,
    /// The `connect()` hostname `canonicalSocketHost` returns for it; `None`
    /// when the bridge refuses the authority before the gate.
    pub socket: Option<String>,
    /// TCP port probed.
    pub port: u16,
    /// Redirect hop probed (the first redirect is hop 1).
    pub hop: u32,
    /// The Rust policy's verdicts with each gate's own spelling of the host
    /// (fetch host for `initial` and `redirect`, socket host for `tcp`;
    /// refused when that spelling is `None`).
    pub rust: Verdicts,
    /// The Rust policy's TCP verdict on the raw host, which the native
    /// mediator judges as the guest wrote it.
    pub rust_raw_tcp: bool,
    /// The probe names a hostname, not an IP literal, so the gates must
    /// agree exactly rather than the JavaScript one being at most as wide.
    pub exact: bool,
}

/// Decodes a policy the host could have emitted, or `None` for input the
/// differential does not compare (not JSON, too long, or a pattern the host
/// would have normalized first).
#[must_use]
pub fn decode(data: &[u8]) -> Option<EgressPolicy> {
    if data.len() > MAX_INPUT_BYTES {
        return None;
    }
    let policy: EgressPolicy = serde_json::from_str(std::str::from_utf8(data).ok()?).ok()?;
    let domains_canonical = policy
        .domains
        .iter()
        .all(|d| normalize_domain_pattern(d).as_deref() == Some(d.as_str()));
    let tcp_canonical = policy
        .tcp
        .iter()
        .all(|g| normalize_tcp_host(&g.host).as_deref() == Some(g.host.as_str()));
    // `try_from_network` stores each grant as `network/prefix`.
    let cidrs_canonical = policy.address_cidrs.iter().all(|c| {
        CidrGrant::parse(c).is_ok_and(|grant| format!("{}/{}", grant.network, grant.prefix) == *c)
    });
    (domains_canonical && tcp_canonical && cidrs_canonical).then_some(policy)
}

/// The policy as the bridge receives it: the `EGRESS_POLICY` JSON text.
#[must_use]
pub fn policy_json(policy: &EgressPolicy) -> String {
    policy.to_policy_json().to_string()
}

/// The bridge source with its workerd-only parts stubbed out, as a classic
/// script whose top-level function declarations (`allowsInitial`,
/// `allowsRedirect`, `allowsTcp`) become globals.
///
/// # Panics
///
/// When `egress.js` no longer has the shape this harness rewrites: the
/// differential must be updated with it, never silently skipped.
#[must_use]
pub fn bridge_script() -> String {
    let import_line = "import { connect } from \"cloudflare:sockets\";";
    let export_line = "export default {";
    assert!(
        EGRESS_JS.contains(import_line) && EGRESS_JS.contains(export_line),
        "egress.js changed shape; update fuzz/oracles/egress_bridge.rs"
    );
    EGRESS_JS
        .replace(
            import_line,
            "const connect = () => { throw new Error(\"no sockets\"); };",
        )
        .replace(export_line, "const bridgeDefault = {")
}

/// The fetch hostname a URL naming `host` carries, as WHATWG URL parsing
/// (workerd's `new URL`, the `url` crate) prints it.
#[must_use]
pub fn fetch_host(host: &str) -> Option<String> {
    let literal = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    url::Host::parse(&literal).ok().map(|h| h.to_string())
}

/// What the bridge's `canonicalSocketHost` returns for a `connect()` host:
/// refused outside `[A-Za-z0-9._:-]`, otherwise the URL parser's spelling.
#[must_use]
pub fn socket_host(host: &str) -> Option<String> {
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'))
    {
        return None;
    }
    fetch_host(host)
}

/// Probe hosts for one policy: [`FIXED_PROBES`], each grant's own name and
/// its near misses, and each CIDR grant's address.
#[must_use]
pub fn probe_hosts(policy: &EgressPolicy) -> Vec<String> {
    let mut hosts: Vec<String> = FIXED_PROBES.iter().map(|h| (*h).to_string()).collect();
    let grants = policy
        .domains
        .iter()
        .chain(policy.tcp.iter().map(|g| &g.host))
        .filter(|p| p.as_str() != ANY_PUBLIC_HOST);
    for pattern in grants {
        let bare = pattern.strip_prefix("*.").unwrap_or(pattern);
        hosts.push(bare.to_string());
        hosts.push(format!("a.{bare}"));
        hosts.push(format!("x{bare}"));
        hosts.push(format!("{bare}."));
        hosts.push(bare.to_ascii_uppercase());
    }
    for cidr in &policy.address_cidrs {
        let addr = cidr.split('/').next().unwrap_or_default().trim();
        hosts.push(addr.to_string());
    }
    hosts.sort();
    hosts.dedup();
    hosts.truncate(MAX_PROBES);
    hosts
}

/// The hop a redirect probe uses: the first redirect, or the last one the
/// policy still follows when it allows none.
#[must_use]
pub fn probe_hop(policy: &EgressPolicy) -> u32 {
    if policy.max_redirects == 0 {
        0
    } else {
        1
    }
}

/// The ports a TCP probe uses: each grant's first port, then 443.
#[must_use]
pub fn probe_ports(policy: &EgressPolicy) -> Vec<u16> {
    let mut ports: Vec<u16> = policy
        .tcp
        .iter()
        .filter_map(|g| g.ports.first().copied())
        .collect();
    ports.push(443);
    ports.sort_unstable();
    ports.dedup();
    ports.truncate(4);
    ports
}

/// The Rust side of every probe for one policy.
#[must_use]
pub fn probes(policy: &EgressPolicy) -> Vec<Probe> {
    let hop = probe_hop(policy);
    let ports = probe_ports(policy);
    let mut out = Vec::new();
    for host in probe_hosts(policy) {
        let fetch = fetch_host(&host);
        let socket = socket_host(&host);
        for &port in &ports {
            let rust = Verdicts {
                initial: fetch.as_deref().is_some_and(|h| policy.allows_initial(h)),
                redirect: fetch
                    .as_deref()
                    .is_some_and(|h| policy.allows_redirect(h, hop)),
                tcp: socket
                    .as_deref()
                    .is_some_and(|h| policy.allows_tcp(h, port)),
            };
            let exact = fetch
                .as_deref()
                .is_some_and(|h| !h.starts_with('[') && h.parse::<std::net::Ipv4Addr>().is_err());
            out.push(Probe {
                host: host.clone(),
                fetch: fetch.clone(),
                socket: socket.clone(),
                port,
                hop,
                rust,
                rust_raw_tcp: policy.allows_tcp(&host, port),
                exact,
            });
        }
    }
    out
}

/// Checks the JavaScript gate's verdicts on one probe against the Rust side.
///
/// # Panics
///
/// When the JavaScript gate allows something the Rust policy refuses, or the
/// two disagree on a hostname probe: the differential's finding.
pub fn check(policy: &EgressPolicy, probe: &Probe, js: Verdicts) {
    let label = || {
        format!(
            "{probe:?} under {} (JavaScript said {js:?})",
            policy_json(policy)
        )
    };
    if policy.mode == NetworkMode::Deny {
        assert!(
            !js.initial && !js.redirect && !js.tcp,
            "deny mode allowed: {}",
            label()
        );
    }
    assert!(
        !js.initial || probe.rust.initial,
        "initial wider than Rust: {}",
        label()
    );
    assert!(
        !js.redirect || probe.rust.redirect,
        "redirect wider than Rust: {}",
        label()
    );
    assert!(
        !js.tcp || probe.rust.tcp,
        "tcp wider than Rust: {}",
        label()
    );
    assert!(
        !js.tcp || probe.rust_raw_tcp,
        "tcp wider than Rust on the raw host: {}",
        label()
    );
    if probe.exact {
        assert_eq!(
            (js.initial, js.redirect),
            (probe.rust.initial, probe.rust.redirect),
            "fetch verdicts differ on a hostname: {}",
            label()
        );
        if probe.socket.is_some() {
            assert_eq!(
                js.tcp,
                probe.rust.tcp,
                "tcp verdicts differ on a hostname: {}",
                label()
            );
        }
    }
}
