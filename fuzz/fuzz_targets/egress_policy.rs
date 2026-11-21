//! Coverage-guided egress allowlist checks (`bookclerk-plugin-manifest`
//! `EgressPolicy`): the only gate on guest network access.
//!
//! The input is the JSON wire form of an `EgressPolicy` (what the host injects
//! into workerd as `EGRESS_POLICY`), so the corpus is readable and the replay
//! test in `bookclerk-plugin-manifest/tests/fuzz_corpus.rs` decodes it the same
//! way. Oracles: deny mode allows nothing; loopback and private destinations
//! are never allowed without an explicit CIDR grant; `*.suffix` never matches
//! `xsuffix`; `*` never matches an IP literal and never grants TCP;
//! percent-encoded hosts never match; hostname normalization is idempotent;
//! the policy survives a JSON round trip; the guest's own `User-Agent` is kept
//! only on hosts an approved non-`*` own-agent pattern matches; a fetch or
//! redirect reaches exactly the ports its `domains` entry grants (its own
//! `:port`, else 80 and 443), judged by an independent entry parser; the
//! isolate tunnel rule is exactly the union of the fetch, redirect and TCP
//! rules on each port; a `localNames` entry widens the address check only to
//! local-network addresses and only for that exact name; a pending setting
//! destination grants nothing and labels only its own host and port.

#![no_main]

use bookclerk_plugin_manifest::{
    canonical_ip, host_matches, is_local_network, normalize_domain_pattern, normalize_hostname,
    normalize_local_name, normalize_tcp_host, split_domain_entry, EgressPolicy, NetworkMode,
    ANY_PUBLIC_HOST,
};
use libfuzzer_sys::fuzz_target;

/// Largest policy document worth decoding.
const MAX_INPUT_BYTES: usize = 8 * 1024;

/// Destinations that must need an explicit CIDR grant.
const RESTRICTED: &[&str] = &[
    "localhost",
    "LOCALHOST.",
    "a.localhost",
    "\u{ff4c}ocalhost",
    "localhost%41",
    "localhost.localdomain",
    "metadata.google.internal",
    "127.0.0.1",
    "127.1.2.3",
    "127.1",
    "2130706433",
    "0x7f.0.0.1",
    "0177.0.0.1",
    "0x7f000001",
    "::1",
    "[::1]",
    "[::ffff:7f00:1]",
    "0.0.0.0",
    "10.0.0.1",
    "172.16.0.1",
    "192.168.1.1",
    "169.254.169.254",
    "fe80::1",
    "::ffff:127.0.0.1",
];

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(policy) = serde_json::from_str::<EgressPolicy>(text) else {
        return;
    };
    check_policy(&policy);
});

/// The port an entry names, parsed without the crate's splitter: `Some(None)`
/// when it names none, `None` when its port is malformed. A port follows the
/// last `:` when the entry is `[…]:port` or holds exactly one colon.
fn oracle_entry_port(entry: &str) -> Option<Option<u16>> {
    let text = if entry.starts_with('[') {
        match entry.find("]:") {
            Some(at) => &entry[at + 2..],
            None if entry.ends_with(']') => return Some(None),
            None => return None,
        }
    } else if entry.matches(':').count() == 1 {
        entry.rsplit(':').next().unwrap_or("")
    } else {
        return Some(None);
    };
    let digits_ok = !text.is_empty()
        && !text.starts_with('0')
        && text.len() <= 5
        && text.bytes().all(|b| b.is_ascii_digit());
    if !digits_ok {
        return None;
    }
    let port: u32 = text.parse().ok()?;
    Some(Some(u16::try_from(port).ok()?))
}

/// Whether `entry` grants `port`: its own port, else 80 and 443.
fn oracle_entry_grants(entry: &str, port: u16) -> bool {
    match oracle_entry_port(entry) {
        Some(Some(granted)) => granted == port,
        Some(None) => port == 80 || port == 443,
        None => false,
    }
}

/// Runs every oracle on one decoded policy.
fn check_policy(policy: &EgressPolicy) {
    let json = serde_json::to_string(policy).expect("policy serializes");
    let again: EgressPolicy = serde_json::from_str(&json).expect("policy re-parses");
    assert_eq!(&again, policy, "JSON round trip changed the policy");
    let _ = policy.to_policy_json();

    let mut hosts: Vec<String> = Vec::new();
    hosts.extend(policy.domains.iter().map(|d| {
        split_domain_entry(d)
            .map_or(d.as_str(), |(host, _)| host)
            .to_string()
    }));
    hosts.extend(policy.tcp.iter().map(|g| g.host.clone()));
    for host in hosts.clone() {
        let bare = host.strip_prefix("*.").unwrap_or(&host).to_string();
        hosts.push(bare.clone());
        hosts.push(format!("a.{bare}"));
        hosts.push(format!("x{bare}"));
        hosts.push(format!("{bare}."));
        hosts.push(bare.to_ascii_uppercase());
        hosts.push(format!("{bare}%41"));
    }
    hosts.extend(RESTRICTED.iter().map(|h| (*h).to_string()));

    for host in &hosts {
        let initial = policy.allows_initial(host);
        let redirect = policy.allows_redirect(host, 0);
        let tcp = policy
            .tcp
            .iter()
            .flat_map(|g| g.ports.iter())
            .any(|&port| policy.allows_tcp(host, port))
            || policy.allows_tcp(host, 443);
        if policy.mode == NetworkMode::Deny {
            assert!(!initial && !redirect && !tcp, "deny mode allowed {host}");
        }
        let mut ports = vec![1_u16, 80, 443, 8443, 65535];
        ports.extend(
            policy
                .domains
                .iter()
                .filter_map(|d| oracle_entry_port(d).flatten()),
        );
        for port in ports {
            // A fetch or redirect reaches a port only when some entry grants
            // it (independent parser of the entry's port), and an entry that
            // plainly covers the host and grants the port is honoured.
            // `host_matches` is the plain IDNA comparison: on an entry already
            // in normalized form it never covers more than the policy's own
            // matching, so it only constrains.
            let some_grants = policy.domains.iter().any(|d| oracle_entry_grants(d, port));
            let plainly_granted = policy.domains.iter().any(|d| {
                normalize_domain_pattern(d).as_deref() == Some(d.as_str())
                    && host_matches(host, d)
                    && oracle_entry_grants(d, port)
            });
            let fetch = policy.allows_fetch(host, port);
            assert!(
                !fetch || (initial && some_grants),
                "fetch widened {host}:{port}"
            );
            assert!(
                fetch || !(initial && plainly_granted),
                "fetch refused a granted {host}:{port}"
            );
            let hop = policy.allows_redirect_to(host, port, 0);
            let undeclared_web =
                policy.allow_undeclared_public_redirects && (port == 80 || port == 443);
            assert!(
                !hop || (redirect && (some_grants || undeclared_web)),
                "redirect widened {host}:{port}"
            );
            assert!(
                hop || !(redirect && (plainly_granted || undeclared_web)),
                "redirect refused a granted {host}:{port}"
            );
            // The isolate tunnel rule is exactly the union of what the
            // isolate's fetch and connect could reach on that port.
            let expected =
                policy.allows_tcp(host, port) || fetch || (policy.max_redirects > 0 && hop);
            assert_eq!(
                policy.allows_isolate_tunnel(host, port),
                expected,
                "isolate tunnel rule is not the union for {host}:{port}"
            );
        }
        if host.contains('%') {
            assert!(!initial && !tcp, "percent-encoded host allowed: {host}");
        }
        if let Some(normalized) = normalize_hostname(host) {
            assert_eq!(
                normalize_hostname(&normalized).as_deref(),
                Some(normalized.as_str()),
                "normalize_hostname is not idempotent on {host}"
            );
        }
    }

    if policy.address_cidrs.is_empty() {
        for host in RESTRICTED {
            // Every spelling the host and URL layers may hand over must be
            // judged as the same destination: trailing dot, surrounding
            // whitespace, upper case and fullwidth digits (IDNA maps them to
            // ASCII, so `１２７.0.0.1` is loopback once normalized).
            for spelling in restricted_spellings(host) {
                assert!(
                    !policy.allows_initial(&spelling),
                    "restricted initial allowed: {spelling:?}"
                );
                assert!(
                    !policy.allows_redirect(&spelling, 0),
                    "restricted redirect allowed: {spelling:?}"
                );
                assert!(
                    !policy.allows_tcp(&spelling, 443),
                    "restricted tcp allowed: {spelling:?}"
                );
            }
            if let Ok(ip) = host.trim_matches(['[', ']']).parse::<std::net::IpAddr>() {
                assert!(!policy.allows_ip(ip), "restricted ip allowed: {host}");
            }
        }
    }

    for pattern in policy
        .domains
        .iter()
        .chain(policy.tcp.iter().map(|g| &g.host))
    {
        if let Some(suffix) = pattern.strip_prefix("*.").and_then(normalize_hostname) {
            assert!(
                host_matches(&suffix, pattern),
                "wildcard misses its own suffix {pattern}"
            );
            assert!(
                host_matches(&format!("a.{suffix}"), pattern),
                "wildcard misses subdomain of {pattern}"
            );
            assert!(
                !host_matches(&format!("x{suffix}"), pattern),
                "wildcard matched sibling of {pattern}"
            );
        }
        assert!(!host_matches(&format!("{pattern}%41"), pattern));
    }

    // `*` grants hostnames only: with no other entry, no IP literal passes
    // the hostname layer, and a `*` TCP grant opens no socket.
    if !policy.domains.is_empty() && policy.domains.iter().all(|d| d == ANY_PUBLIC_HOST) {
        for host in ["8.8.8.8", "1.1.1.1", "[2606:4700::1111]", "134744072"] {
            assert!(
                !policy.allows_initial(host),
                "`*` allowed IP literal {host}"
            );
        }
        assert!(host_matches("example.com", ANY_PUBLIC_HOST));
    }
    if policy.tcp.iter().all(|g| g.host == ANY_PUBLIC_HOST) {
        for host in &hosts {
            assert!(
                !policy
                    .tcp
                    .iter()
                    .flat_map(|g| g.ports.iter())
                    .any(|&port| policy.allows_tcp(host, port)),
                "`*` TCP grant opened {host}"
            );
        }
    }

    assert!(
        !policy.allows_redirect("example.com", policy.max_redirects),
        "hop budget exceeded"
    );
    check_user_agent(policy);
    check_local_names(policy);
    check_pending_settings(policy);
}

/// Pending setting destinations grant nothing and only label a refusal:
/// every rule answers the same with them removed, and a pending lookup
/// returns only an entry with that exact port and (case-insensitively) host.
fn check_pending_settings(policy: &EgressPolicy) {
    let without = EgressPolicy {
        pending_settings: Vec::new(),
        ..policy.clone()
    };
    let mut probes: Vec<(String, u16)> = policy
        .pending_settings
        .iter()
        .map(|p| (p.host.clone(), p.port))
        .collect();
    probes.push(("example.com".into(), 443));
    probes.push(("127.0.0.1".into(), 9));
    for (host, port) in &probes {
        assert_eq!(
            policy.allows_fetch(host, *port),
            without.allows_fetch(host, *port),
            "pending entry changed fetch for {host}:{port}"
        );
        assert_eq!(
            policy.allows_tcp(host, *port),
            without.allows_tcp(host, *port),
            "pending entry changed tcp for {host}:{port}"
        );
        assert_eq!(
            policy.allows_redirect_to(host, *port, 0),
            without.allows_redirect_to(host, *port, 0),
            "pending entry changed redirects for {host}:{port}"
        );
        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            assert_eq!(
                policy.allows_ip_for(host, ip),
                without.allows_ip_for(host, ip),
                "pending entry changed the address check for {host}"
            );
        }
        if let Some(found) = policy.pending_setting_for(host, *port) {
            assert_eq!(found.port, *port, "pending lookup returned another port");
            let bare = host
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .unwrap_or(host);
            let wanted = normalize_hostname(bare).expect("lookup host normalized");
            assert!(
                found.host.eq_ignore_ascii_case(&wanted),
                "pending lookup returned another host: {} for {host}",
                found.host
            );
        }
        assert!(
            without.pending_setting_for(host, *port).is_none(),
            "lookup found an entry in an empty list"
        );
    }
}
/// Plain lowercase ASCII form of `raw` (trimmed, trailing dots dropped), or
/// `None` when it holds anything an IDNA step could reinterpret.
fn plain_ascii(raw: &str) -> Option<String> {
    let text = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    let plain = !text.is_empty()
        && text
            .split('.')
            .all(|label| !label.is_empty() && !label.starts_with("xn--"))
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    plain.then_some(text)
}

/// Independent own-agent decision for plain ASCII hosts and patterns, written
/// without the crate's normalizers: exact match, or a `*.` suffix that matches
/// the bare suffix and its subdomains; `*` never matches. `None` when any
/// input is outside that plain subset.
fn plain_keeps_own_agent(host: &str, patterns: &[String]) -> Option<bool> {
    let host = plain_ascii(host)?;
    let mut keeps = false;
    for raw in patterns {
        let pattern = raw.trim().trim_end_matches('.');
        if pattern == "*" {
            continue;
        }
        keeps |= match pattern.strip_prefix("*.") {
            Some(suffix) => {
                let suffix = plain_ascii(suffix)?;
                host == suffix || host.ends_with(&format!(".{suffix}"))
            }
            None => host == plain_ascii(pattern)?,
        };
    }
    Some(keeps)
}

/// User-agent oracle: the guest's own agent is kept exactly on hosts an
/// approved non-`*` pattern matches, never via `*`, never for a
/// percent-encoded host, and every other request carries the host's agent.
fn check_user_agent(policy: &EgressPolicy) {
    let mut hosts: Vec<String> = vec!["example.com".into(), "127.0.0.1".into(), "localhost".into()];
    for pattern in policy
        .own_user_agent_hosts
        .iter()
        .chain(policy.domains.iter())
    {
        let bare = pattern.strip_prefix("*.").unwrap_or(pattern).to_string();
        hosts.push(format!("a.{bare}"));
        hosts.push(format!("x{bare}"));
        hosts.push(format!("{bare}%41"));
        hosts.push(bare);
    }
    let agent = policy.effective_user_agent();
    for host in &hosts {
        let expected = policy
            .own_user_agent_hosts
            .iter()
            .filter_map(|p| normalize_tcp_host(p))
            .any(|p| host_matches(host, &p));
        assert_eq!(
            policy.keeps_own_user_agent(host),
            expected,
            "own user-agent decision for {host:?}"
        );
        if let Some(independent) = plain_keeps_own_agent(host, &policy.own_user_agent_hosts) {
            assert_eq!(expected, independent, "plain oracle for {host:?}");
        }
        if host.contains('%') {
            assert!(
                !policy.keeps_own_user_agent(host),
                "percent host kept {host:?}"
            );
        }
        assert_eq!(policy.user_agent_for(host, None), agent);
        let sent = policy.user_agent_for(host, Some("guest/1"));
        assert_eq!(
            sent == "guest/1",
            expected || agent == "guest/1",
            "{host:?}"
        );
    }
    if policy
        .own_user_agent_hosts
        .iter()
        .all(|p| normalize_domain_pattern(p).is_none_or(|n| n == ANY_PUBLIC_HOST))
    {
        for host in &hosts {
            assert!(!policy.keeps_own_user_agent(host), "`*` kept {host:?}");
        }
    }
}

/// Spellings of one restricted destination that must all be refused without a
/// CIDR grant: as given, with a trailing dot, padded, upper-cased, and with
/// ASCII digits replaced by their fullwidth forms.
fn restricted_spellings(host: &str) -> Vec<String> {
    let fullwidth: String = host
        .chars()
        .map(|c| match c {
            '0'..='9' => char::from_u32(0xff10 + (u32::from(c) - u32::from('0'))).unwrap_or(c),
            other => other,
        })
        .collect();
    vec![
        host.to_string(),
        format!("{host}."),
        format!(" {host}. "),
        host.to_ascii_uppercase(),
        fullwidth,
    ]
}

/// A name widens the address check only to local-network addresses, and only
/// for a name listed (in normalized form) in `local_names`; a listed literal
/// widens only to itself; loopback, link-local and other special addresses
/// are never widened.
fn check_local_names(policy: &EgressPolicy) {
    let addresses: Vec<std::net::IpAddr> = [
        "192.168.1.20",
        "10.0.0.1",
        "172.16.0.1",
        "100.64.0.1",
        "fd00::1",
        "::ffff:10.0.0.1",
        "127.0.0.1",
        "::1",
        "169.254.169.254",
        "fe80::1",
        "0.0.0.0",
        "224.0.0.1",
        "1.1.1.1",
    ]
    .iter()
    .filter_map(|text| text.parse().ok())
    .collect();
    let mut names: Vec<String> = policy.local_names.clone();
    names.extend(policy.local_names.iter().map(|n| format!("a.{n}")));
    names.extend(
        [
            "localhost",
            "example.com",
            "10.0.0.1",
            "[fd00::1]",
            "192.168.1.20",
        ]
        .map(String::from),
    );
    for name in &names {
        let listed = normalize_local_name(name).is_some_and(|normalized| {
            policy
                .local_names
                .iter()
                .any(|entry| normalize_local_name(entry).as_deref() == Some(normalized.as_str()))
        });
        for &ip in &addresses {
            let allowed = policy.allows_ip_for(name, ip);
            // A literal entry binds exactly its own address.
            let same_address = name
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .map_or(true, |literal| canonical_ip(literal) == canonical_ip(ip));
            let expected = policy.allows_ip(ip)
                || (policy.mode == NetworkMode::Outbound
                    && listed
                    && same_address
                    && is_local_network(ip));
            assert_eq!(allowed, expected, "allows_ip_for({name:?}, {ip})");
        }
    }
}
