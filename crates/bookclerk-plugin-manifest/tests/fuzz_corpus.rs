//! Replays the checked-in `plugin_manifest`, `egress_policy`, `cron_schedule`
//! and `https_url_policy` fuzz corpora
//! in PR CI (no libFuzzer required), with the same oracles as the targets, so
//! an input that once broke a manifest or policy check stays fixed
//! (`docs/adr/untrusted-input-fuzzing.md`).
// Test code: a failed assertion or a missing corpus file is the test failing,
// not a documented panic path.
#![allow(clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

use bookclerk_plugin_manifest::{
    canonical_ip, consent_domains_for, format_manifest, host_matches, is_local_network,
    normalize_domain_pattern, normalize_hostname, normalize_local_name, normalize_tcp_host, parse,
    split_domain_entry, EgressPolicy, NetworkMode, ANY_PUBLIC_HOST,
};

/// Regular files directly under `fuzz/corpus/<target>/`, canonicalized and
/// confined to that directory.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn corpus_files(target: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus")
        .join(target)
        .canonicalize()
        .expect("canonicalize fuzz corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fuzz corpus")
        .map(|entry| entry.expect("entry").path())
        // Inline canonicalize → starts_with at each sink (CodeQL two-state barrier).
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&dir) && path.is_file())
        .collect();
    files.sort();
    assert!(!files.is_empty(), "empty corpus for {target}");
    files
}

/// Spellings of one restricted destination that must all be refused without a
/// CIDR grant (mirrors the target): as given, trailing dot, padded, upper case,
/// and ASCII digits replaced by their fullwidth forms, which IDNA maps back.
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

/// Destinations that must need an explicit CIDR grant (mirrors the target).
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

#[test]
fn fuzz_corpus_plugin_manifest_round_trips() {
    let mut parsed = 0;
    for path in corpus_files("plugin_manifest") {
        let text = std::fs::read_to_string(&path).expect("read");
        let Ok(manifest) = parse(&text) else {
            continue;
        };
        parsed += 1;
        manifest.validate().expect("parse() validated");
        let formatted = format_manifest(&manifest)
            .unwrap_or_else(|err| panic!("format {}: {err}", path.display()));
        assert_eq!(
            parse(&formatted).expect("reparse"),
            manifest,
            "{}",
            path.display()
        );
        let _ = consent_domains_for(&manifest);
        let policy = EgressPolicy::from_manifest(&manifest);
        if manifest.capabilities.network.mode == NetworkMode::Deny {
            assert_eq!(policy.mode, NetworkMode::Deny, "{}", path.display());
        }
        let _ = policy.to_policy_json();
    }
    assert!(parsed > 0, "no plugin_manifest seed parsed");
}

#[test]
fn fuzz_corpus_egress_policy_holds_invariants() {
    let mut parsed = 0;
    for path in corpus_files("egress_policy") {
        let text = std::fs::read_to_string(&path).expect("read");
        let Ok(policy) = serde_json::from_str::<EgressPolicy>(&text) else {
            continue;
        };
        parsed += 1;
        let json = serde_json::to_string(&policy).expect("serialize");
        assert_eq!(
            serde_json::from_str::<EgressPolicy>(&json).expect("reparse"),
            policy
        );
        if policy.mode == NetworkMode::Deny {
            for host in RESTRICTED
                .iter()
                .copied()
                .chain(policy.domains.iter().map(String::as_str))
            {
                assert!(
                    !policy.allows_initial(host)
                        && !policy.allows_tcp(host, 443)
                        && !policy.allows_redirect(host, 0),
                    "deny mode allowed {host:?} via {}",
                    path.display()
                );
            }
        }
        assert!(
            !policy.allows_redirect("example.com", policy.max_redirects),
            "hop budget exceeded via {}",
            path.display()
        );
        if policy.address_cidrs.is_empty() {
            for host in RESTRICTED
                .iter()
                .flat_map(|host| restricted_spellings(host))
            {
                assert!(
                    !policy.allows_initial(&host),
                    "{host:?} via {}",
                    path.display()
                );
                assert!(
                    !policy.allows_tcp(&host, 443),
                    "{host:?} via {}",
                    path.display()
                );
                assert!(
                    !policy.allows_redirect(&host, 0),
                    "{host:?} via {}",
                    path.display()
                );
                for port in policy.tcp.iter().flat_map(|grant| grant.ports.iter()) {
                    assert!(
                        !policy.allows_tcp(&host, *port),
                        "{host:?}:{port} via {}",
                        path.display()
                    );
                }
            }
        }
        for pattern in policy
            .domains
            .iter()
            .chain(policy.tcp.iter().map(|g| &g.host))
        {
            if let Some(suffix) = pattern.strip_prefix("*.") {
                if let Some(normalized) = normalize_hostname(suffix) {
                    assert_eq!(
                        normalize_hostname(&normalized).as_deref(),
                        Some(normalized.as_str())
                    );
                    assert!(host_matches(suffix, pattern));
                    assert!(host_matches(&format!("a.{normalized}"), pattern));
                    assert!(!host_matches(&format!("x{suffix}"), pattern));
                }
            }
            assert!(!policy.allows_initial(&format!("{pattern}%41")));
        }
        // `*` grants hostnames only, and never a socket.
        if !policy.domains.is_empty() && policy.domains.iter().all(|d| d == ANY_PUBLIC_HOST) {
            for host in ["8.8.8.8", "1.1.1.1", "[2606:4700::1111]", "134744072"] {
                assert!(
                    !policy.allows_initial(host),
                    "`*` allowed {host} via {}",
                    path.display()
                );
            }
        }
        for grant in policy.tcp.iter().filter(|g| g.host == ANY_PUBLIC_HOST) {
            for port in &grant.ports {
                assert!(
                    !policy.allows_tcp("example.com", *port),
                    "`*` TCP grant opened a socket via {}",
                    path.display()
                );
            }
        }
        check_user_agent(&policy);
        check_ports(&policy);
        check_local_names(&policy);
    }
    assert!(parsed > 0, "no egress_policy seed parsed");
}

/// The port an entry names, parsed without the crate's splitter (mirrors the
/// target): `Some(None)` when it names none, `None` when malformed.
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

/// Fetch and redirect reach exactly the ports their entry grants, and the
/// isolate tunnel rule is their union with TCP on each port (mirrors the
/// target).
fn check_ports(policy: &EgressPolicy) {
    let mut hosts: Vec<String> = Vec::new();
    for entry in &policy.domains {
        let host = split_domain_entry(entry).map_or(entry.as_str(), |(host, _)| host);
        let bare = host.strip_prefix("*.").unwrap_or(host);
        hosts.push(host.to_string());
        hosts.push(bare.to_string());
        hosts.push(format!("a.{bare}"));
        if bare.contains(':') {
            hosts.push(format!("[{bare}]"));
        }
    }
    let mut ports = vec![1_u16, 80, 443, 8443, 65535];
    ports.extend(
        policy
            .domains
            .iter()
            .filter_map(|d| oracle_entry_port(d).flatten()),
    );
    for host in &hosts {
        let initial = policy.allows_initial(host);
        let redirect = policy.allows_redirect(host, 0);
        for &port in &ports {
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
            let expected =
                policy.allows_tcp(host, port) || fetch || (policy.max_redirects > 0 && hop);
            assert_eq!(
                policy.allows_isolate_tunnel(host, port),
                expected,
                "isolate tunnel rule is not the union for {host}:{port}"
            );
        }
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

/// The `cron_schedule` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/cron_schedule.rs"]
mod cron_oracle;

#[test]
fn fuzz_corpus_cron_schedule_replays() {
    for path in corpus_files("cron_schedule") {
        let bytes = std::fs::read(&path).expect("read");
        cron_oracle::check(&bytes);
    }
}

/// The `https_url_policy` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/https_url_policy.rs"]
mod https_url_oracle;

#[test]
fn fuzz_corpus_https_url_policy_replays() {
    let mut accepted = 0;
    for path in corpus_files("https_url_policy") {
        let bytes = std::fs::read(&path).expect("read");
        https_url_oracle::check(&bytes);
        let text = String::from_utf8_lossy(&bytes);
        if bookclerk_plugin_manifest::https_url_on_hosts(&text, https_url_oracle::PATTERNS)
            .is_some()
        {
            accepted += 1;
        }
    }
    assert!(accepted > 0, "no https_url_policy seed was accepted");
}

#[test]
fn fuzz_corpus_table_setting_seed_parses_and_round_trips() {
    let path = corpus_files("plugin_manifest")
        .into_iter()
        .find(|path| path.ends_with("table-setting.toml"))
        .expect("table-setting.toml seed");
    let text = std::fs::read_to_string(&path).expect("read");
    let manifest = parse(&text).expect("the table seed is a valid manifest");
    let spec = manifest
        .settings
        .iter()
        .find(|spec| spec.key == "mappings")
        .expect("mappings setting");
    assert_eq!(spec.columns.len(), 4);
    let formatted = format_manifest(&manifest).expect("format");
    assert_eq!(parse(&formatted).expect("reparse"), manifest);
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

#[path = "../../../fuzz/oracles/setting_destination.rs"]
mod setting_destination_oracle;

// Test code: an unreadable corpus file is a failed test.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_setting_destination_holds_the_oracle() {
    for path in corpus_files("setting_destination") {
        let data = std::fs::read(&path).expect("read");
        setting_destination_oracle::check(&data);
    }
}
