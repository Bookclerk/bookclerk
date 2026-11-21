//! Shared oracle for the `setting_destination` fuzz target and the
//! `bookclerk-plugin-manifest` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! A network setting's value is untrusted text that becomes part of a
//! plugin's network grant once approved. Byte 0 selects the form: bit 0
//! picks `tcp` (set) or `fetch`, bit 1 says bytes 1-2 hold a big-endian
//! `default_port`; the rest is the value. The invariants:
//!
//! - derivation never panics, and an accepted destination has a port in
//!   1-65535 and a host with no wildcard, percent sign or whitespace;
//! - it is a fixed point: deriving its own display form gives it back;
//! - the grant pieces it adds are already in normalized form: the `domains`
//!   entry, the TCP host and the local-name entry each normalize to
//!   themselves, and the `domains` entry names exactly its port;
//! - its scope matches an independent reading of the host: a local address
//!   is a local-network literal, a public address passes the public check,
//!   a name is neither an IP literal nor a restricted name, and only names
//!   and local addresses add a local-name entry;
//! - a value carrying `@` before the host never derives (no credentials);
//! - a name holds only letters, digits, `-`, `_` and dots.

use bookclerk_plugin_abi::SettingNetwork;
use bookclerk_plugin_manifest::{
    derive_setting_destination, is_local_network, is_public_internet, is_restricted_hostname,
    normalize_domain_pattern, normalize_local_name, normalize_tcp_host, split_domain_entry,
    DestinationScope,
};

/// Runs every invariant on one input.
pub fn check(data: &[u8]) {
    let Some((&head, rest)) = data.split_first() else {
        return;
    };
    let network = if head & 1 == 1 {
        SettingNetwork::Tcp
    } else {
        SettingNetwork::Fetch
    };
    let (default_port, rest) = if head & 2 == 2 && rest.len() >= 2 {
        (Some(u16::from_be_bytes([rest[0], rest[1]])), &rest[2..])
    } else {
        (None, rest)
    };
    let Ok(value) = std::str::from_utf8(rest) else {
        return;
    };
    let Ok(found) = derive_setting_destination(network, default_port, value) else {
        return;
    };
    assert_ne!(found.port, 0, "port 0 from {value:?}");
    assert!(
        !found.host.is_empty()
            && !found
                .host
                .contains(|ch: char| ch == '*' || ch == '%' || ch.is_whitespace()),
        "bad host {:?} from {value:?}",
        found.host
    );
    let authority = value
        .trim()
        .split_once("://")
        .map_or(value.trim(), |(_, rest)| rest);
    let authority = authority.split(['/', '?', '#']).next().unwrap_or("");
    assert!(
        !authority.contains('@'),
        "credentials accepted in {value:?}"
    );

    let again = derive_setting_destination(network, None, &found.display())
        .unwrap_or_else(|err| panic!("display {:?} does not derive: {err}", found.display()));
    assert_eq!(again, found, "not a fixed point for {value:?}");

    match found.domain_entry() {
        Some(entry) => {
            assert_eq!(
                normalize_domain_pattern(&entry).as_deref(),
                Some(entry.as_str())
            );
            assert_eq!(
                split_domain_entry(&entry).map(|(_, port)| port),
                Some(Some(found.port))
            );
        }
        None => assert_eq!(network, SettingNetwork::Tcp),
    }
    if let Some(grant) = found.tcp_grant() {
        assert_eq!(
            normalize_tcp_host(&grant.host).as_deref(),
            Some(grant.host.as_str())
        );
        assert_eq!(grant.ports, vec![found.port]);
    }
    if let Some(name) = found.local_name() {
        assert_eq!(normalize_local_name(&name).as_deref(), Some(name.as_str()));
    }

    let literal = found.host.parse::<std::net::IpAddr>().ok();
    match found.scope {
        DestinationScope::Name => {
            assert!(literal.is_none() && !is_restricted_hostname(&found.host));
            assert!(found
                .host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')));
            assert!(found.local_name().is_some());
        }
        DestinationScope::PublicAddress => {
            assert!(literal.is_some_and(is_public_internet));
            assert!(found.local_name().is_none());
        }
        DestinationScope::LocalAddress => {
            assert!(literal.is_some_and(is_local_network));
            assert!(found.local_name().is_some());
        }
        DestinationScope::Restricted => {
            assert!(
                literal.is_some_and(|ip| !is_public_internet(ip) && !is_local_network(ip))
                    || is_restricted_hostname(&found.host)
            );
            assert!(found.local_name().is_none());
        }
    }
}
