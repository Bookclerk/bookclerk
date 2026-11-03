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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct OwnerInfo {
    /// The library's cluster id.
    #[schema(example = "c1f2e3d4-5a6b-4c7d-8e9f-0a1b2c3d4e5f")]
    pub cluster_id: String,
    /// A name to show for the library.
    #[schema(example = "books.example.com")]
    pub library_name: String,
    /// The library's canonical origin: the one the node handshake signs and
    /// the authorize endpoint lives under.
    #[schema(example = "https://books.example.com")]
    pub origin: String,
    /// The node API transport mode (`acme`, `proxy`, `self_signed`,
    /// `loopback`).
    #[schema(example = "acme")]
    pub transport: String,
    /// Whether the library lets new branches join (`core.host_enrollment`).
    #[schema(example = true)]
    pub enrollment: bool,
    /// In `self_signed` mode: base64url SHA-256 of the certificate's
    /// SubjectPublicKeyInfo, which the joining operator confirms against
    /// the owner's Settings → Branches page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "kQ3nUj9Z2vY4xW8pT6rS1aB5cD7eF0gH2iJ4kL6mN8o")]
    pub spki_fingerprint: Option<String>,
    /// The library's Bookclerk version.
    #[serde(default)]
    #[schema(example = "0.4.0")]
    pub software_version: String,
    /// The library's ancestry: its own cluster id first, then the cluster
    /// id of every library above it up to the root. A joining library
    /// refuses a parent whose ancestry names its own cluster id, so a
    /// tree of libraries never becomes a cycle.
    #[serde(default)]
    #[schema(example = json!(["c1f2e3d4-5a6b-4c7d-8e9f-0a1b2c3d4e5f"]))]
    pub ancestry: Vec<String>,
}

/// `POST /api/node/v1/enroll/oauth` body. Keys are base64url without
/// padding.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OAuthEnrollRequest {
    /// The authorization code from the callback.
    #[schema(example = "BhD5jGe21A8WEIm7SrdBZ-KXhs5B9HkeUSEc58grX94")]
    pub code: String,
    /// The PKCE verifier.
    #[schema(example = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk")]
    pub code_verifier: String,
    /// The redirect URI the authorization was requested with.
    #[schema(example = "http://127.0.0.1:41923/enroll/callback")]
    pub redirect_uri: String,
    /// Ed25519 public signing key.
    #[schema(example = "Nflg3bNZ7h6Q7aePwVccVzry1BxzaqtguKtJlkfPE0M")]
    pub sign_pk: String,
    /// X25519 public box key.
    #[schema(example = "hSDwCYkwp1R0i33ctD73Wg2_Og0mOBr066SpjqqbTmo")]
    pub box_pk: String,
    /// The name this branch joins as (the node's label).
    #[serde(default)]
    #[schema(example = "study-mini")]
    pub branch_name: String,
    /// The joining install's Bookclerk version.
    #[serde(default)]
    #[schema(example = "0.4.0")]
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
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct OAuthEnrollAnswer {
    /// The library's cluster id.
    #[schema(example = "c1f2e3d4-5a6b-4c7d-8e9f-0a1b2c3d4e5f")]
    pub cluster_id: String,
    /// The node id recorded for the joining install.
    #[schema(example = "7d2b9e4c-1f3a-4b8d-9c6e-2a5f8b1d4e7c")]
    pub node_id: String,
    /// The connection type recorded (`branch` when left out).
    #[serde(default, skip_serializing_if = "ConnectionType::is_branch")]
    #[schema(example = "clerk")]
    pub connection: ConnectionType,
    /// For a clerk: the [`crate::ClerkBootstrap`] sealed to the node's box
    /// key ([`crate::seal_clerk_bootstrap`]), as base64url without padding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(
        example = "Qm9va2NsZXJrIHNlYWxlZCBjbGVyayBib290c3RyYXA6IGVwaGVtZXJhbCBrZXkgKyBjaXBoZXJ0ZXh0"
    )]
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
    fn enroll_request_debug_names_the_request_but_not_its_secrets() {
        let request = OAuthEnrollRequest {
            code: "the-code".into(),
            code_verifier: "the-verifier".into(),
            redirect_uri: "http://127.0.0.1:41923/enroll/callback".into(),
            sign_pk: "sign".into(),
            box_pk: "box".into(),
            branch_name: "study-mini".into(),
            software_version: "0.4.0".into(),
            connection: ConnectionType::Branch,
        };
        let shown = format!("{request:?}");
        assert!(shown.starts_with("OAuthEnrollRequest {"), "{shown}");
        assert!(
            shown.contains("http://127.0.0.1:41923/enroll/callback"),
            "{shown}"
        );
        assert!(shown.contains("study-mini"), "{shown}");
        assert!(!shown.contains("the-code"), "{shown}");
        assert!(!shown.contains("the-verifier"), "{shown}");
    }

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
