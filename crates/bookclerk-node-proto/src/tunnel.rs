//! What a request on the `tunnel` transport may reach.
//!
//! A relay ingress plugin hands the library requests from the public
//! internet through its `ORIGIN` binding (see `docs/adr/ingress-plugins.md`).
//! The daemon routes them to the same router as its listeners, so this
//! policy is an authorization boundary over paths the host did not produce.
//! It is kept free of I/O so the `tunnel_policy` fuzz target and the unit
//! tests drive the same code the daemon does.
//!
//! - [`normalize_tunnel_path`]: the path the router will match, after the
//!   router's own trailing-slash normalization, so the policy and the router
//!   always judge the same path.
//! - [`tunnel_refusal`]: operator-only paths (the operator-token sign-in,
//!   the tray handoff, operator elevation and first-Owner bootstrap) are refused whatever is exposed;
//!   with [`TunnelExposure::Branches`] only what branches and clerks need is
//!   served: the node API, the clerk and copy routes, sign-in, OIDC and the
//!   web app's pages, never the rest of `/api/` or the legacy control routes.

/// What public traffic may reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelExposure {
    /// Only what branches and clerks need.
    Branches,
    /// The whole library, web app included.
    Library,
}

/// Why a tunnel request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelRefusal {
    /// The path is for the operator on this machine only.
    OperatorOnly,
    /// The exposure the operator chose does not include the path.
    NotExposed,
}

impl TunnelRefusal {
    /// Stable code for the response body and logs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OperatorOnly => "operator_only",
            Self::NotExposed => "not_exposed",
        }
    }
}

/// Paths no tunnel request reaches: the operator-token sign-in, the loopback
/// tray handoff, operator elevation and the operator's first-Owner
/// bootstrap. Each also covers the paths below it.
pub const OPERATOR_ONLY_PATHS: [&str; 6] = [
    "/api/auth/login",
    "/api/auth/tray-handoff",
    "/api/auth/elevate",
    "/api/auth/passkeys/elevate",
    "/api/auth/oidc/elevate",
    "/api/auth/bootstrap",
];

/// API paths [`TunnelExposure::Branches`] serves. Each also covers the paths
/// below it.
pub const BRANCH_API_PATHS: [&str; 13] = [
    "/api/node",
    "/api/clerk",
    "/api/copies",
    "/api/auth/signin",
    "/api/auth/password",
    "/api/auth/logout",
    "/api/auth/me",
    "/api/auth/passkeys/login",
    "/api/auth/totp/login",
    "/api/auth/oidc/login",
    "/api/auth/oidc/callback",
    "/api/auth/branch-consent",
    "/api/auth/clerk-consent",
];

/// Legacy control routes outside `/api/` that
/// [`TunnelExposure::Branches`] refuses. Each also covers the paths below
/// it.
pub const LEGACY_CONTROL_PATHS: [&str; 5] =
    ["/status", "/scan", "/acquire", "/jobs", "/integrations"];

/// The path the router matches for `raw`: the router trims trailing slashes
/// and keeps exactly one leading slash, so `//api/node/` routes as
/// `/api/node`.
///
/// # Arguments
///
/// * `raw` - The request target's path, without the query.
///
/// # Returns
///
/// `raw` without leading or trailing slashes, behind one leading slash.
#[must_use]
pub fn normalize_tunnel_path(raw: &str) -> String {
    format!("/{}", raw.trim_matches('/'))
}

/// Whether a tunnel request for `path` is refused.
///
/// # Arguments
///
/// * `path` - The path from [`normalize_tunnel_path`].
/// * `exposure` - What the operator chose to expose.
///
/// # Returns
///
/// `None` to serve the request, or why it is refused.
#[must_use]
pub fn tunnel_refusal(path: &str, exposure: TunnelExposure) -> Option<TunnelRefusal> {
    if OPERATOR_ONLY_PATHS.iter().any(|base| under(path, base)) {
        return Some(TunnelRefusal::OperatorOnly);
    }
    match exposure {
        TunnelExposure::Library => None,
        TunnelExposure::Branches => {
            let api = under(path, "/api");
            let allowed = if api {
                BRANCH_API_PATHS.iter().any(|base| under(path, base))
            } else {
                !LEGACY_CONTROL_PATHS.iter().any(|base| under(path, base))
            };
            (!allowed).then_some(TunnelRefusal::NotExposed)
        }
    }
}

/// True when `path` is `base` or a path below it (`base` followed by `/`).
fn under(path: &str, base: &str) -> bool {
    path.strip_prefix(base)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizing_matches_the_router() {
        assert_eq!(normalize_tunnel_path(""), "/");
        assert_eq!(normalize_tunnel_path("/"), "/");
        assert_eq!(
            normalize_tunnel_path("//api/node/v1/info//"),
            "/api/node/v1/info"
        );
        assert_eq!(normalize_tunnel_path("/api//node"), "/api//node");
    }

    #[test]
    fn operator_paths_are_refused_whatever_is_exposed() {
        for exposure in [TunnelExposure::Branches, TunnelExposure::Library] {
            for path in [
                "/api/auth/login",
                "/api/auth/tray-handoff",
                "/api/auth/tray-handoff/prepare",
                "/api/auth/elevate",
                "/api/auth/passkeys/elevate/begin",
                "/api/auth/oidc/elevate",
                "/api/auth/bootstrap",
            ] {
                assert_eq!(
                    tunnel_refusal(path, exposure),
                    Some(TunnelRefusal::OperatorOnly),
                    "{path}"
                );
            }
        }
    }

    #[test]
    fn branches_exposure_serves_only_what_branches_need() {
        let branches = TunnelExposure::Branches;
        for path in [
            "/",
            "/health",
            "/assets/app.js",
            "/signin",
            "/api/node/v1/connect",
            "/api/clerk/v1/token",
            "/api/copies/v1/x/files/0",
            "/api/auth/signin",
            "/api/auth/password",
            "/api/auth/passkeys/login/begin",
            "/api/auth/branch-consent/step-up",
            "/oidc/authorize",
            "/.well-known/openid-configuration",
        ] {
            assert_eq!(tunnel_refusal(path, branches), None, "{path}");
        }
        for path in [
            "/api",
            "/api/library/works",
            "/api/users",
            "/api/auth/passkeys",
            "/api/auth/sessions",
            "/api/nodes",
            "/api/node-other",
            "/status",
            "/scan",
            "/jobs/1/cancel",
            "/integrations/x/scan",
        ] {
            assert_eq!(
                tunnel_refusal(path, branches),
                Some(TunnelRefusal::NotExposed),
                "{path}"
            );
        }
    }

    #[test]
    fn library_exposure_serves_everything_else() {
        for path in ["/api/library/works", "/status", "/api/users", "/"] {
            assert_eq!(
                tunnel_refusal(path, TunnelExposure::Library),
                None,
                "{path}"
            );
        }
    }
}
