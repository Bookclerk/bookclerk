//! Coverage-guided `plugin.toml` parsing and the policies derived from it
//! (`bookclerk-plugin-manifest`). A manifest comes from a third-party package,
//! so everything it feeds (validation, egress policy, consent domains, the
//! canonical formatter) is attacker-shaped.
//!
//! Oracles: no panic; a manifest that parses formats to text that parses back
//! to an equal manifest; a derived egress policy never widens past the
//! manifest's declared mode.

#![no_main]

use bookclerk_plugin_manifest::{
    consent_domains_for, format_manifest, parse, EgressPolicy, NetworkMode,
};
use libfuzzer_sys::fuzz_target;

/// Largest manifest worth parsing; real ones are a few KiB.
const MAX_INPUT_BYTES: usize = 32 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(manifest) = parse(text) else {
        return;
    };
    manifest
        .validate()
        .expect("parse() validated; validate() must agree");
    let _ = manifest.primary_family();

    if let Ok(formatted) = format_manifest(&manifest) {
        let reparsed = parse(&formatted).expect("canonical format must parse");
        assert_eq!(
            reparsed, manifest,
            "format/parse round trip changed the manifest"
        );
    }

    let _ = consent_domains_for(&manifest);
    let policy = EgressPolicy::from_manifest(&manifest);
    let strict = EgressPolicy::try_from_manifest(&manifest);
    if manifest.capabilities.network.mode == NetworkMode::Deny {
        assert_eq!(
            policy.mode,
            NetworkMode::Deny,
            "deny manifest produced an outbound policy"
        );
        assert!(policy.domains.is_empty() && policy.tcp.is_empty());
        if let Ok(strict) = &strict {
            assert_eq!(strict.mode, NetworkMode::Deny);
        }
    }
    let _ = policy.to_policy_json();
});
