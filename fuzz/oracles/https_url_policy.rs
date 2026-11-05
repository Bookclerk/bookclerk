//! Shared oracle for the `https_url_policy` fuzz target and the
//! `bookclerk-plugin-manifest` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is a URL from untrusted plugin output (a metadata provider's
//! cover URL) that the host would fetch itself. It is checked against the
//! Audible image CDN patterns the acquire overlay uses. The invariants,
//! checked on the accepted URL's text by an independent byte-level reading
//! rather than by the URL parser:
//!
//! - checking never panics, and an accepted URL is accepted again unchanged
//!   (the check is idempotent);
//! - an accepted URL is `https://`, its authority has no user info, port or
//!   IP literal, its host has no empty label, and it is a CDN suffix or a
//!   subdomain of one;
//! - an input that, once surrounding whitespace is trimmed, does not start
//!   with `https:` (ignoring case) or has a control byte or space left in it
//!   is refused.

use bookclerk_plugin_manifest::{https_url_on_hosts, MAX_FETCH_URL_BYTES};

/// Largest input worth checking: just past the policy's own cap.
pub const MAX_INPUT_BYTES: usize = MAX_FETCH_URL_BYTES + 64;

/// The patterns the acquire overlay passes.
pub const PATTERNS: &[&str] = &["*.media-amazon.com", "*.ssl-images-amazon.com"];

/// Host suffixes those patterns cover, for the independent reading.
const SUFFIXES: &[&str] = &["media-amazon.com", "ssl-images-amazon.com"];

/// Runs every invariant on one input; panics on a violation.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Some(accepted) = https_url_on_hosts(text, PATTERNS) else {
        return;
    };
    let lead = text.trim();
    assert!(
        !lead.bytes().any(|b| b <= b' ' || b == 0x7f),
        "accepted an input with an inner control byte or space {text:?}"
    );
    assert!(
        lead.get(..6)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https:")),
        "accepted a non-https input {text:?}"
    );
    assert_eq!(
        https_url_on_hosts(&accepted, PATTERNS).as_deref(),
        Some(accepted.as_str()),
        "not idempotent for {text:?}"
    );
    let rest = accepted
        .strip_prefix("https://")
        .unwrap_or_else(|| panic!("accepted {accepted:?} is not https://"));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    assert!(
        !authority.contains(['@', ':', '[', ']', '%']),
        "authority {authority:?} carries user info, a port or an IP literal"
    );
    let host = authority.strip_suffix('.').unwrap_or(authority);
    assert!(
        !host.split('.').any(str::is_empty),
        "host {host:?} has an empty label for {text:?}"
    );
    assert!(
        SUFFIXES.iter().any(|suffix| {
            host == *suffix
                || host
                    .strip_suffix(suffix)
                    .is_some_and(|label| label.ends_with('.') && label.len() > 1)
        }),
        "host {host:?} is off the CDN for {text:?}"
    );
}
