//! Client sign-in: an install that shows a person's books from a library
//! without joining it as a node.
//!
//! A client signs in as the person, never as a machine. It uses the same
//! OAuth 2.0 authorization code flow with PKCE as a joining branch
//! ([`crate::BRANCH_CLIENT_ID`]), through a second built-in public client,
//! [`CLIENT_CLIENT_ID`], with the scope [`CLIENT_SCOPE`]:
//!
//! 1. The client daemon reads [`crate::INFO_PATH`].
//! 2. It sends the browser to the library's `/oidc/authorize` with
//!    `client_id = bookclerk-client`, `scope = library:member`, an S256
//!    challenge, a `state`, a `client_name`, and a loopback or https
//!    redirect URI ([`crate::is_branch_redirect_uri`]).
//! 3. The person signs in on the library and consents; the library
//!    redirects back with a single-use code.
//! 4. The daemon redeems the code with its verifier at [`CLIENT_TOKEN_PATH`]
//!    ([`ClientTokenRequest`]) and gets a session token
//!    ([`ClientTokenAnswer`]).
//!
//! The daemon then sends the token as
//! `Authorization: Bookclerk-Client <token>` ([`CLIENT_AUTH_SCHEME`],
//! [`parse_client_authorization`]). The library accepts it only on routes a
//! library session may use for the person's own books, never on
//! administration, and never as a cookie. No node row, node key or node
//! socket is involved.

use serde::{Deserialize, Serialize};

/// The built-in public OAuth client a client install signs in through. It
/// is never stored in the library's clients table.
pub const CLIENT_CLIENT_ID: &str = "bookclerk-client";

/// The only scope [`CLIENT_CLIENT_ID`] may ask for.
pub const CLIENT_SCOPE: &str = "library:member";

/// `POST`: redeem an approved client sign-in for a session token.
pub const CLIENT_TOKEN_PATH: &str = "/api/client/v1/token";

/// The `Authorization` scheme a client sends its session token under.
pub const CLIENT_AUTH_SCHEME: &str = "Bookclerk-Client";

/// Seconds an authorization code for [`CLIENT_CLIENT_ID`] lives.
pub const CLIENT_CODE_TTL_SECS: i64 = 60;

/// Days a client session stays signed in without being used. Each use
/// moves the expiry forward again.
pub const CLIENT_IDLE_DAYS: i64 = 90;

/// Longest client name sent.
pub const MAX_CLIENT_NAME_BYTES: usize = 64;

/// Length of a session token: base64url (no padding) of 32 random bytes.
pub const CLIENT_TOKEN_LEN: usize = 43;

/// `POST /api/client/v1/token` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ClientTokenRequest {
    /// The single-use code from the redirect.
    #[schema(example = "q8Zl0m3VnS2xR7bW4cYd1eTf6gUh9iJk5LoNpQrStUv")]
    pub code: String,
    /// The PKCE verifier whose S256 challenge the authorize request sent.
    #[schema(example = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk")]
    pub code_verifier: String,
    /// The redirect URI the authorize request named.
    #[schema(example = "http://127.0.0.1:41923/client/callback")]
    pub redirect_uri: String,
}

/// `POST /api/client/v1/token` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ClientTokenAnswer {
    /// The session token, sent as `Authorization: Bookclerk-Client <token>`.
    /// Shown once; the library keeps only its hash.
    #[schema(example = "Zm9vYmFyYmF6cXV4cXV1eGNvcmdlZ3JhdWx0Z2FycGx5")]
    pub token: String,
    /// The library's cluster id.
    #[schema(example = "c1f2e3d4-5a6b-4c7d-8e9f-0a1b2c3d4e5f")]
    pub cluster_id: String,
    /// The signed-in person on that library.
    pub user: ClientUser,
    /// Seconds the session stays signed in unused; each use resets it.
    #[schema(example = 7_776_000)]
    pub idle_secs: i64,
}

/// The person a client session signs in as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ClientUser {
    /// The user's id on the library.
    #[schema(example = 7)]
    pub id: i64,
    /// The user's display name on the library, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "Riley")]
    pub display_name: Option<String>,
}

/// True for a client name: 1 to [`MAX_CLIENT_NAME_BYTES`] bytes, no control
/// characters.
#[must_use]
pub fn is_client_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.len() <= MAX_CLIENT_NAME_BYTES
        && !name.chars().any(char::is_control)
}

/// True for a session token: exactly [`CLIENT_TOKEN_LEN`] base64url
/// characters.
#[must_use]
pub fn is_client_token(token: &str) -> bool {
    token.len() == CLIENT_TOKEN_LEN
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The session token in an `Authorization` header value, when the value is
/// exactly `Bookclerk-Client <token>`.
///
/// The scheme is matched case-insensitively (RFC 9110 section 11.1); one
/// space separates it from the token; the token must pass
/// [`is_client_token`]. Anything else, including an operator `Bearer`
/// token, is `None`.
///
/// # Arguments
///
/// * `value` - The raw header value bytes.
///
/// # Returns
///
/// The token, borrowed from `value`, or `None`.
#[must_use]
pub fn parse_client_authorization(value: &[u8]) -> Option<&str> {
    let value = std::str::from_utf8(value).ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case(CLIENT_AUTH_SCHEME) {
        return None;
    }
    is_client_token(token).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "Zm9vYmFyYmF6cXV4cXV1eGNvcmdlZ3JhdWx0Z2FycGx";

    #[test]
    fn a_token_parses_under_its_scheme_only() {
        assert_eq!(TOKEN.len(), CLIENT_TOKEN_LEN);
        let header = format!("Bookclerk-Client {TOKEN}");
        assert_eq!(parse_client_authorization(header.as_bytes()), Some(TOKEN));
        let lower = format!("bookclerk-client {TOKEN}");
        assert_eq!(parse_client_authorization(lower.as_bytes()), Some(TOKEN));
        for refused in [
            format!("Bearer {TOKEN}"),
            format!("Bookclerk-Client  {TOKEN}"),
            format!("Bookclerk-Client {TOKEN} "),
            format!("Bookclerk-Client {TOKEN}x"),
            format!("Bookclerk-Client{TOKEN}"),
            String::from("Bookclerk-Client "),
            String::from("Bookclerk-Client"),
            format!("Bookclerk-Node {TOKEN}"),
        ] {
            assert_eq!(
                parse_client_authorization(refused.as_bytes()),
                None,
                "{refused}"
            );
        }
        assert_eq!(
            parse_client_authorization(b"Bookclerk-Client \xff\xfe"),
            None
        );
    }

    #[test]
    fn tokens_are_base64url_of_fixed_length() {
        assert!(is_client_token(TOKEN));
        assert!(!is_client_token(&TOKEN[1..]));
        assert!(!is_client_token(&format!("{}+", &TOKEN[1..])));
        assert!(!is_client_token(&format!("{}=", &TOKEN[1..])));
    }

    #[test]
    fn client_names_are_short_and_printable() {
        assert!(is_client_name("Riley's laptop"));
        assert!(!is_client_name(""));
        assert!(!is_client_name("   "));
        assert!(!is_client_name("a\nb"));
        assert!(!is_client_name(&"x".repeat(MAX_CLIENT_NAME_BYTES + 1)));
        assert!(is_client_name(&"x".repeat(MAX_CLIENT_NAME_BYTES)));
    }
}
