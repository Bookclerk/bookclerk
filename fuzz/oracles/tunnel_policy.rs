//! Shared oracle for the `tunnel_policy` fuzz target and the
//! `bookclerk-node-proto` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! A relay ingress plugin hands the library requests from the public
//! internet, and [`tunnel_refusal`] decides from the path alone whether one
//! reaches the router. The input is one byte choosing the exposure (even:
//! branches, odd: the whole library) followed by the request path as sent.
//!
//! Whatever the path is:
//!
//! - [`normalize_tunnel_path`] gives what the router matches (one leading
//!   slash, no trailing slash) and is idempotent, so the policy never
//!   judges a path the router reads differently;
//! - the operator-only paths are refused under both exposures;
//! - the whole-library exposure refuses nothing else;
//! - under the branches exposure, an `/api/` path is served only below one
//!   of the branch API bases, and any other path is served unless it is a
//!   legacy control route;
//! - the answer equals the one worked out here from the path's segments
//!   rather than string prefixes, so the policy and the oracle would have
//!   to be wrong the same way.

use bookclerk_node_proto::{
    normalize_tunnel_path, tunnel_refusal, TunnelExposure, TunnelRefusal, BRANCH_API_PATHS,
    LEGACY_CONTROL_PATHS, OPERATOR_ONLY_PATHS,
};

/// Largest input worth checking: the exposure byte and a path far longer
/// than any route.
pub const MAX_INPUT_BYTES: usize = 1 + 512;

/// What a checked input was answered with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The input was not a path (not UTF-8, empty or too long).
    Skipped,
    /// The router sees the request.
    Served,
    /// Refused for the operator only.
    OperatorOnly,
    /// Refused as not exposed.
    NotExposed,
}

/// The segments of a normalized path: the parts between slashes after the
/// leading one (`/` has none).
fn segments(path: &str) -> Vec<&str> {
    match path.strip_prefix('/') {
        Some("") | None => Vec::new(),
        Some(rest) => rest.split('/').collect(),
    }
}

/// True when the segments of `path` begin with those of `base`.
fn below(path: &[&str], base: &str) -> bool {
    let base = segments(base);
    path.len() >= base.len() && path[..base.len()] == base[..]
}

/// The verdict, read a second way: by segments.
fn expected(path: &str, exposure: TunnelExposure) -> Verdict {
    let parts = segments(path);
    if OPERATOR_ONLY_PATHS.iter().any(|base| below(&parts, base)) {
        return Verdict::OperatorOnly;
    }
    if exposure == TunnelExposure::Library {
        return Verdict::Served;
    }
    let served = if parts.first() == Some(&"api") {
        BRANCH_API_PATHS.iter().any(|base| below(&parts, base))
    } else {
        !LEGACY_CONTROL_PATHS.iter().any(|base| below(&parts, base))
    };
    if served {
        Verdict::Served
    } else {
        Verdict::NotExposed
    }
}

/// Checks one input against every invariant.
///
/// # Returns
///
/// What the policy answered.
///
/// # Panics
///
/// Panics when an invariant does not hold.
pub fn check(data: &[u8]) -> Verdict {
    let Some((&choice, raw)) = data.split_first() else {
        return Verdict::Skipped;
    };
    if raw.len() > MAX_INPUT_BYTES {
        return Verdict::Skipped;
    }
    let Ok(raw) = std::str::from_utf8(raw) else {
        return Verdict::Skipped;
    };
    let exposure = if choice % 2 == 0 {
        TunnelExposure::Branches
    } else {
        TunnelExposure::Library
    };
    let path = normalize_tunnel_path(raw);
    assert!(
        path.starts_with('/'),
        "normalized path {path:?} lost its slash"
    );
    assert!(
        path == "/" || !path.ends_with('/'),
        "normalized path {path:?} keeps a trailing slash"
    );
    assert!(
        !path.starts_with("//"),
        "normalized path {path:?} keeps two leading slashes"
    );
    assert_eq!(
        normalize_tunnel_path(&path),
        path,
        "normalizing is not idempotent"
    );
    let verdict = match tunnel_refusal(&path, exposure) {
        None => Verdict::Served,
        Some(TunnelRefusal::OperatorOnly) => Verdict::OperatorOnly,
        Some(TunnelRefusal::NotExposed) => Verdict::NotExposed,
    };
    // The operator-only answer never depends on the exposure.
    let other = match exposure {
        TunnelExposure::Branches => TunnelExposure::Library,
        TunnelExposure::Library => TunnelExposure::Branches,
    };
    assert_eq!(
        verdict == Verdict::OperatorOnly,
        tunnel_refusal(&path, other) == Some(TunnelRefusal::OperatorOnly),
        "operator-only answer for {path:?} depends on the exposure"
    );
    assert_eq!(
        verdict,
        expected(&path, exposure),
        "policy disagrees for {path:?}"
    );
    verdict
}
