//! Coverage-guided `plugin.toml` parsing and the policies derived from it
//! (`bookclerk-plugin-manifest`). A manifest comes from a third-party package,
//! so everything it feeds (validation, egress policy, consent domains, the
//! canonical formatter) is attacker-shaped.
//!
//! Oracles: no panic; a manifest that parses formats to text that parses back
//! to an equal manifest; a derived egress policy never widens past the
//! manifest's declared mode; no binding takes the reserved `NETWORK` name;
//! a validated network setting is a string with a nonzero port (if any), only
//! a network setting carries `default_port`, and a destination derived from
//! its default names a host and a nonzero port.

#![no_main]

use bookclerk_plugin_abi::SettingKind;
use bookclerk_plugin_manifest::{
    consent_domains_for, derive_setting_destination, format_manifest, parse, EgressPolicy,
    NetworkMode, NETWORK_BINDING,
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

    // A manifest that parsed and validated must format, or the round-trip
    // oracle below is skipped exactly where the formatter regressed.
    let formatted = format_manifest(&manifest).expect("validated manifest must format");
    let reparsed = parse(&formatted).expect("canonical format must parse");
    assert_eq!(
        reparsed, manifest,
        "format/parse round trip changed the manifest"
    );

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

    assert!(
        !manifest
            .binding_names()
            .iter()
            .any(|n| n == NETWORK_BINDING)
            && !manifest
                .databases
                .iter()
                .any(|d| d.binding == NETWORK_BINDING),
        "a binding took the reserved NETWORK name"
    );
    for setting in &manifest.settings {
        if setting.default_port.is_some() {
            assert!(setting.network.is_some(), "default_port without network");
        }
        let Some(network) = setting.network else {
            continue;
        };
        assert_eq!(setting.kind, SettingKind::String, "network on a non-string");
        assert_ne!(setting.default_port, Some(0), "zero default_port");
        let Some(value) = setting.default.as_ref().and_then(|v| v.as_str()) else {
            continue;
        };
        if let Ok(destination) = derive_setting_destination(network, setting.default_port, value) {
            assert_eq!(destination.network, network);
            assert_ne!(destination.port, 0, "derived a zero port");
            assert!(!destination.host.is_empty(), "derived an empty host");
            let _ = destination.display();
        }
    }
});
