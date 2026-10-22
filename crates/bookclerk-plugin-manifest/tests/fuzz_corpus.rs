//! Replays the checked-in `plugin_manifest` and `egress_policy` fuzz corpora
//! in PR CI (no libFuzzer required), with the same oracles as the targets, so
//! an input that once broke a manifest or policy check stays fixed
//! (`docs/adr/untrusted-input-fuzzing.md`).
// Test code: a failed assertion or a missing corpus file is the test failing,
// not a documented panic path.
#![allow(clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

use bookclerk_plugin_manifest::{
    consent_domains_for, format_manifest, host_matches, normalize_hostname, parse, EgressPolicy,
    NetworkMode,
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
    "127.0.0.1",
    "::1",
    "[::1]",
    "10.0.0.1",
    "192.168.1.1",
    "169.254.169.254",
];

#[test]
fn fuzz_corpus_plugin_manifest_round_trips() {
    for path in corpus_files("plugin_manifest") {
        let text = std::fs::read_to_string(&path).expect("read");
        let Ok(manifest) = parse(&text) else {
            continue;
        };
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
}

#[test]
fn fuzz_corpus_egress_policy_holds_invariants() {
    for path in corpus_files("egress_policy") {
        let text = std::fs::read_to_string(&path).expect("read");
        let Ok(policy) = serde_json::from_str::<EgressPolicy>(&text) else {
            continue;
        };
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
                assert!(!policy.allows_initial(host) && !policy.allows_tcp(host, 443));
            }
        }
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
                    assert!(!host_matches(&format!("x{suffix}"), pattern));
                }
            }
            assert!(!policy.allows_initial(&format!("{pattern}%41")));
        }
    }
}
