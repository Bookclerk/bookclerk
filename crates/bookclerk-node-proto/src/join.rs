//! Joining a library with a sign-in: the OAuth 2.0 authorization code flow
//! with PKCE (RFC 7636) against the owner's built-in authorization server,
//! for one public client, [`BRANCH_CLIENT_ID`].
//!
//! 1. The joining daemon reads [`INFO_PATH`] ([`OwnerInfo`]).
//! 2. It sends the browser to the owner's `/oidc/authorize` with
//!    `client_id = bookclerk-branch`, `scope = node:enroll`, an S256
//!    challenge, a `state`, and its own `/join/callback` as the redirect
//!    URI ([`is_branch_redirect_uri`]: a loopback origin, RFC 8252, or any
//!    https origin).
//! 3. The owner's user signs in and consents; the owner redirects back with
//!    a single-use code.
//! 4. The daemon redeems the code with its verifier and the node's public
//!    keys at [`ENROLL_OAUTH_PATH`] ([`OAuthEnrollRequest`]).
//!
//! The verifier, `state` and node private keys never leave the joining
//! daemon.

use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use crate::clerk::ConnectionType;

/// The built-in public OAuth client a joining branch signs in through. It
/// is never stored in the owner's clients table.
pub const BRANCH_CLIENT_ID: &str = "bookclerk-branch";

/// The only scope [`BRANCH_CLIENT_ID`] may ask for.
pub const BRANCH_SCOPE: &str = "node:enroll";

/// `GET`: what a library tells a joining install before sign-in.
pub const INFO_PATH: &str = "/api/node/v1/info";

/// `POST`: redeem an approved branch sign-in.
pub const ENROLL_OAUTH_PATH: &str = "/api/node/v1/enroll/oauth";

/// The owner's authorization endpoint (path).
pub const AUTHORIZE_PATH: &str = "/oidc/authorize";

/// Path of the joining install's own callback (the SPA route the browser
/// returns to, and the CLI's loopback listener).
pub const JOIN_CALLBACK_PATH: &str = "/join/callback";

/// Seconds an authorization code for [`BRANCH_CLIENT_ID`] lives.
pub const BRANCH_CODE_TTL_SECS: i64 = 60;

/// Longest branch name sent.
pub const MAX_BRANCH_NAME_BYTES: usize = 64;

/// Longest library name accepted: a library is named by its public host,
/// and a DNS name runs to 253 bytes.
pub const MAX_LIBRARY_NAME_BYTES: usize = 253;

/// `GET /api/node/v1/info` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerInfo {
    /// The library's cluster id.
    pub cluster_id: String,
    /// A name to show for the library.
    pub library_name: String,
    /// The library's canonical origin: the one the node handshake signs and
    /// the authorize endpoint lives under.
    pub origin: String,
    /// The node API transport mode (`acme`, `proxy`, `self_signed`,
    /// `loopback`).
    pub transport: String,
    /// Whether the library lets new branches join (`core.host_enrollment`).
    pub enrollment: bool,
    /// In `self_signed` mode: base64url SHA-256 of the certificate's
    /// SubjectPublicKeyInfo, which the joining operator confirms against
    /// the owner's Settings → Branches page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spki_fingerprint: Option<String>,
    /// The library's Bookclerk version.
    #[serde(default)]
    pub software_version: String,
    /// The library's ancestry: its own cluster id first, then the cluster
    /// id of every library above it up to the root. A joining library
    /// refuses a parent whose ancestry names its own cluster id, so a
    /// tree of libraries never becomes a cycle.
    #[serde(default)]
    pub ancestry: Vec<String>,
}

/// `POST /api/node/v1/enroll/oauth` body. Keys are base64url without
/// padding.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthEnrollRequest {
    /// The authorization code from the callback.
    pub code: String,
    /// The PKCE verifier.
    pub code_verifier: String,
    /// The redirect URI the authorization was requested with.
    pub redirect_uri: String,
    /// Ed25519 public signing key.
    pub sign_pk: String,
    /// X25519 public box key.
    pub box_pk: String,
    /// The name this branch joins as (the node's label).
    #[serde(default)]
    pub branch_name: String,
    /// The joining install's Bookclerk version.
    #[serde(default)]
    pub software_version: String,
    /// The connection type the code was approved for. It must match the
    /// one bound to the code at consent; left out for a branch.
    #[serde(default, skip_serializing_if = "ConnectionType::is_branch")]
    pub connection: ConnectionType,
}

impl std::fmt::Debug for OAuthEnrollRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthEnrollRequest")
            .field("redirect_uri", &self.redirect_uri)
            .field("branch_name", &self.branch_name)
            .field("connection", &self.connection)
            .finish_non_exhaustive()
    }
}

/// `POST /api/node/v1/enroll/oauth` answer.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthEnrollAnswer {
    /// The library's cluster id.
    pub cluster_id: String,
    /// The node id recorded for the joining install.
    pub node_id: String,
    /// The connection type recorded (`branch` when left out).
    #[serde(default, skip_serializing_if = "ConnectionType::is_branch")]
    pub connection: ConnectionType,
    /// For a clerk: the [`crate::ClerkBootstrap`] sealed to the node's box
    /// key ([`crate::seal_clerk_bootstrap`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap: Option<String>,
}

impl std::fmt::Debug for OAuthEnrollAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthEnrollAnswer")
            .field("cluster_id", &self.cluster_id)
            .field("node_id", &self.node_id)
            .field("connection", &self.connection)
            .field("bootstrap", &self.bootstrap.as_ref().map(|_| "<sealed>"))
            .finish()
    }
}

/// The RFC 7636 S256 challenge of `verifier`: base64url (no padding) of its
/// SHA-256.
#[must_use]
pub fn pkce_challenge(verifier: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier))
}

/// True for a redirect URI [`BRANCH_CLIENT_ID`] may use: an absolute
/// `https` URL, or an `http` URL whose host is a loopback IP literal or
/// `localhost` (RFC 8252 section 7.3), with no user info and no fragment.
#[must_use]
pub fn is_branch_redirect_uri(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return false;
    }
    match url.scheme() {
        "https" => url.host().is_some(),
        "http" => match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
            None => false,
        },
        _ => false,
    }
}

/// True for a library name: 1 to [`MAX_LIBRARY_NAME_BYTES`] bytes, no
/// control characters.
#[must_use]
pub fn is_library_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.len() <= MAX_LIBRARY_NAME_BYTES
        && !name.chars().any(char::is_control)
}

/// True for a branch name: 1 to [`MAX_BRANCH_NAME_BYTES`] bytes, no control
/// characters.
#[must_use]
pub fn is_branch_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.len() <= MAX_BRANCH_NAME_BYTES
        && !name.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_uris_are_loopback_or_https_only() {
        for ok in [
            "http://127.0.0.1:41234/join/callback",
            "http://[::1]:8787/join/callback",
            "http://localhost:8787/join/callback",
            "https://host.example/join/callback",
            "https://192.168.1.4:8443/join/callback",
        ] {
            assert!(is_branch_redirect_uri(ok), "{ok}");
        }
        for bad in [
            "http://192.168.1.4:8787/join/callback",
            "http://host.example/join/callback",
            "http://localhost.evil.example/cb",
            "https://user:pw@host.example/cb",
            "https://host.example/cb#frag",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "/join/callback",
            "",
        ] {
            assert!(!is_branch_redirect_uri(bad), "{bad}");
        }
    }

    #[test]
    fn the_challenge_is_rfc_7636_s256() {
        // RFC 7636 appendix B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn enroll_requests_never_print_their_secrets() {
        let request = OAuthEnrollRequest {
            code: "the-code".into(),
            code_verifier: "the-verifier".into(),
            redirect_uri: "http://localhost/join/callback".into(),
            sign_pk: "s".into(),
            box_pk: "b".into(),
            branch_name: "den".into(),
            software_version: "1".into(),
            connection: ConnectionType::Branch,
        };
        // A branch request stays the shape older libraries accept.
        assert!(!serde_json::to_string(&request)
            .unwrap()
            .contains("connection"));
        let printed = format!("{request:?}");
        assert!(!printed.contains("the-code") && !printed.contains("the-verifier"));
        assert!(is_branch_name("den"));
        assert!(!is_branch_name(" "));
        assert!(!is_branch_name("a\u{7}b"));
    }
}
