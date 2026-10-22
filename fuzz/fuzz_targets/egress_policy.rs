//! Coverage-guided egress allowlist checks (`bookclerk-plugin-manifest`
//! `EgressPolicy`): the only gate on guest network access.
//!
//! The input is the JSON wire form of an `EgressPolicy` (what the host injects
//! into workerd as `EGRESS_POLICY`), so the corpus is readable and the replay
//! test in `bookclerk-plugin-manifest/tests/fuzz_corpus.rs` decodes it the same
//! way. Oracles: deny mode allows nothing; loopback and private destinations
//! are never allowed without an explicit CIDR grant; `*.suffix` never matches
//! `xsuffix`; percent-encoded hosts never match; hostname normalization is
//! idempotent; the policy survives a JSON round trip.

#![no_main]

use bookclerk_plugin_manifest::{host_matches, normalize_hostname, EgressPolicy, NetworkMode};
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

/// Runs every oracle on one decoded policy.
fn check_policy(policy: &EgressPolicy) {
    let json = serde_json::to_string(policy).expect("policy serializes");
    let again: EgressPolicy = serde_json::from_str(&json).expect("policy re-parses");
    assert_eq!(&again, policy, "JSON round trip changed the policy");
    let _ = policy.to_policy_json();

    let mut hosts: Vec<String> = Vec::new();
    hosts.extend(policy.domains.iter().cloned());
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

    assert!(
        !policy.allows_redirect("example.com", policy.max_redirects),
        "hop budget exceeded"
    );
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
