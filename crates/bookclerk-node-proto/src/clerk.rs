//! Clerk sign-in: an install that shows a person's books from a library
//! without joining it as a node.
//!
//! A clerk signs in as the person, never as a machine. It uses the same
//! OAuth 2.0 authorization code flow with PKCE as a joining branch
//! ([`crate::BRANCH_CLIENT_ID`]), through a second built-in public client,
//! [`CLERK_CLIENT_ID`], with the scope [`CLERK_SCOPE`]:
//!
//! 1. The clerk's daemon reads [`crate::INFO_PATH`].
//! 2. It sends the browser to the library's `/oidc/authorize` with
//!    `client_id = bookclerk-clerk`, `scope = library:member`, an S256
//!    challenge, a `state`, a `clerk_name`, and a loopback or https
//!    redirect URI ([`crate::is_branch_redirect_uri`]).
//! 3. The person signs in on the library and consents; the library
//!    redirects back with a single-use code.
//! 4. The daemon redeems the code with its verifier at [`CLERK_TOKEN_PATH`]
//!    ([`ClerkTokenRequest`]) and gets a session token
//!    ([`ClerkTokenAnswer`]).
//!
//! The daemon then sends the token as
//! `Authorization: Bookclerk-Clerk <token>` ([`CLERK_AUTH_SCHEME`],
//! [`parse_clerk_authorization`]). The library accepts it only on routes a
//! library session may use for the person's own books, never on
//! administration, and never as a cookie. No node row, node key or node
//! socket is involved.

use serde::{Deserialize, Serialize};

use crate::is_branch_redirect_uri;

/// The built-in public OAuth client a clerk signs in through. It
/// is never stored in the library's clients table.
pub const CLERK_CLIENT_ID: &str = "bookclerk-clerk";

/// The only scope [`CLERK_CLIENT_ID`] may ask for.
pub const CLERK_SCOPE: &str = "library:member";

/// `POST`: redeem an approved clerk sign-in for a session token.
pub const CLERK_TOKEN_PATH: &str = "/api/clerk/v1/token";

/// The clerk's own web app page a clerk sign-in's redirect comes back to
/// (`<clerk origin>/clerk/callback?code=…&state=…`).
pub const CLERK_CALLBACK_PATH: &str = "/clerk/callback";

/// The `Authorization` scheme a clerk sends its session token under.
pub const CLERK_AUTH_SCHEME: &str = "Bookclerk-Clerk";

/// Seconds an authorization code for [`CLERK_CLIENT_ID`] lives.
pub const CLERK_CODE_TTL_SECS: i64 = 60;

/// Days a clerk session stays signed in without being used. Each use
/// moves the expiry forward again.
pub const CLERK_IDLE_DAYS: i64 = 90;

/// Longest clerk name sent.
pub const MAX_CLERK_NAME_BYTES: usize = 64;

/// Length of a session token: base64url (no padding) of 32 random bytes.
pub const CLERK_TOKEN_LEN: usize = 43;

/// Longest `state` an authorize request may carry.
pub const MAX_CLERK_STATE_BYTES: usize = 512;

/// Length of an S256 code challenge: base64url (no padding) of a SHA-256.
const S256_CHALLENGE_LEN: usize = 43;

/// `POST /api/clerk/v1/token` body: exactly these three strings, each
/// once, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ClerkTokenRequest {
    /// The single-use code from the redirect.
    #[schema(example = "q8Zl0m3VnS2xR7bW4cYd1eTf6gUh9iJk5LoNpQrStUv")]
    pub code: String,
    /// The PKCE verifier whose S256 challenge the authorize request sent.
    #[schema(example = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk")]
    pub code_verifier: String,
    /// The redirect URI the authorize request named.
    #[schema(example = "http://127.0.0.1:41923/clerk/callback")]
    pub redirect_uri: String,
}

/// `POST /api/clerk/v1/token` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ClerkTokenAnswer {
    /// The session token, sent as `Authorization: Bookclerk-Clerk <token>`.
    /// Shown once; the library keeps only its hash.
    #[schema(example = "Zm9vYmFyYmF6cXV4cXV1eGNvcmdlZ3JhdWx0Z2FycGx5")]
    pub token: String,
    /// The library id.
    #[schema(example = "c1f2e3d4-5a6b-4c7d-8e9f-0a1b2c3d4e5f")]
    pub library_id: String,
    /// The signed-in person on that library.
    pub user: ClerkUser,
    /// Seconds the session stays signed in unused; each use resets it.
    #[schema(example = 7_776_000)]
    pub idle_secs: i64,
}

/// The person a clerk session signs in as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ClerkUser {
    /// The user's id on the library.
    #[schema(example = 7)]
    pub id: i64,
    /// The user's display name on the library, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "Riley")]
    pub display_name: Option<String>,
}

/// True for a clerk name: 1 to [`MAX_CLERK_NAME_BYTES`] bytes, no control
/// characters.
#[must_use]
pub fn is_clerk_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.len() <= MAX_CLERK_NAME_BYTES
        && !name.chars().any(char::is_control)
}

/// The authorization request a clerk consent is for (the query
/// `/oidc/authorize` forwards to the library's `/clerk/approve` page).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, utoipa::IntoParams, utoipa::ToSchema)]
#[into_params(parameter_in = Query)]
pub struct ClerkAuthorizeParams {
    /// Must be [`CLERK_CLIENT_ID`].
    #[schema(example = "bookclerk-clerk")]
    pub client_id: String,
    /// Where the code goes: loopback or https.
    #[schema(example = "http://127.0.0.1:41923/clerk/callback")]
    pub redirect_uri: String,
    /// Must be `code`.
    #[schema(example = "code")]
    pub response_type: String,
    /// Must be [`CLERK_SCOPE`].
    #[serde(default)]
    #[schema(example = "library:member")]
    pub scope: Option<String>,
    /// Echoed on the redirect.
    #[serde(default)]
    #[schema(example = "c3RhdGUtOGY2ZTJkMWE")]
    pub state: Option<String>,
    /// PKCE S256 challenge.
    #[schema(example = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM")]
    pub code_challenge: String,
    /// Must be `S256`.
    #[serde(default)]
    #[schema(example = "S256")]
    pub code_challenge_method: Option<String>,
    /// The name the clerk gives itself, shown on the consent page
    /// and in the user's session list.
    #[serde(default)]
    #[schema(example = "Riley's laptop")]
    pub clerk_name: Option<String>,
}

/// Parses an authorize query string (the raw query `/oidc/authorize`
/// received), sans-IO so the fuzz target drives the same code.
///
/// # Arguments
///
/// * `raw` - The query string, without the `?`.
///
/// # Returns
///
/// The parameters, or `None` when a parameter repeats, a required one is
/// missing, or the shape is wrong. Unknown parameters are ignored.
#[must_use]
pub fn parse_clerk_query(raw: &str) -> Option<ClerkAuthorizeParams> {
    let mut map = serde_json::Map::new();
    for (name, value) in url::form_urlencoded::parse(raw.as_bytes()) {
        if map
            .insert(
                name.into_owned(),
                serde_json::Value::String(value.into_owned()),
            )
            .is_some()
        {
            return None;
        }
    }
    serde_json::from_value(serde_json::Value::Object(map)).ok()
}

/// Why a clerk authorization request is malformed, or `None` when the
/// library may show its consent page.
///
/// # Arguments
///
/// * `params` - The parsed request ([`parse_clerk_query`]).
///
/// # Returns
///
/// A short reason, or `None`.
#[must_use]
pub fn clerk_request_problem(params: &ClerkAuthorizeParams) -> Option<&'static str> {
    if params.client_id != CLERK_CLIENT_ID {
        return Some("unknown client");
    }
    if params.response_type != "code" {
        return Some("response_type must be code");
    }
    if params.code_challenge_method.as_deref() != Some("S256")
        || !is_s256_challenge(&params.code_challenge)
    {
        return Some("an S256 code challenge is required");
    }
    if params.scope.as_deref().map(str::trim) != Some(CLERK_SCOPE) {
        return Some("scope must be library:member");
    }
    if !is_branch_redirect_uri(&params.redirect_uri) {
        return Some("the redirect URI must be a loopback or https URL");
    }
    if !params.clerk_name.as_deref().is_some_and(is_clerk_name) {
        return Some("the clerk needs a name of 1 to 64 characters");
    }
    if params.state.as_deref().is_some_and(|state| {
        state.len() > MAX_CLERK_STATE_BYTES || state.chars().any(char::is_control)
    }) {
        return Some("malformed state");
    }
    None
}

/// True for an S256 code challenge: exactly 43 base64url characters.
#[must_use]
pub fn is_s256_challenge(raw: &str) -> bool {
    raw.len() == S256_CHALLENGE_LEN
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// True for a session token: exactly [`CLERK_TOKEN_LEN`] base64url
/// characters.
#[must_use]
pub fn is_clerk_token(token: &str) -> bool {
    token.len() == CLERK_TOKEN_LEN
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The session token in an `Authorization` header value, when the value is
/// exactly `Bookclerk-Clerk <token>`.
///
/// The scheme is matched case-insensitively (RFC 9110 section 11.1); one
/// space separates it from the token; the token must pass
/// [`is_clerk_token`]. Anything else, including an operator `Bearer`
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
pub fn parse_clerk_authorization(value: &[u8]) -> Option<&str> {
    let value = std::str::from_utf8(value).ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case(CLERK_AUTH_SCHEME) {
        return None;
    }
    is_clerk_token(token).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "Zm9vYmFyYmF6cXV4cXV1eGNvcmdlZ3JhdWx0Z2FycGx";

    #[test]
    fn a_token_request_is_one_object_only() {
        let object = br#" {"code":"c","code_verifier":"v","redirect_uri":"r"}"#;
        assert_eq!(
            parse_clerk_token_request(object),
            Some(ClerkTokenRequest {
                code: "c".to_string(),
                code_verifier: "v".to_string(),
                redirect_uri: "r".to_string(),
            })
        );
        for refused in [
            &br#"["c","v","r"]"#[..],
            br#"{"code":"c","code":"d","code_verifier":"v","redirect_uri":"r"}"#,
            br#"{"code":"c","code_verifier":"v","redirect_uri":"r","x":1}"#,
            b"",
        ] {
            assert_eq!(parse_clerk_token_request(refused), None);
        }
    }

    #[test]
    fn a_token_answer_is_checked_before_it_is_kept() {
        let good = format!(
            r#"{{"token":"{TOKEN}","library_id":"c-1","user":{{"id":7,"display_name":"Riley"}},"idle_secs":7776000,"later":true}}"#
        );
        let answer = parse_clerk_token_answer(good.as_bytes()).expect("a good answer");
        assert_eq!(answer.token, TOKEN);
        assert_eq!(answer.library_id, "c-1");
        assert_eq!(answer.user.id, 7);
        assert_eq!(answer.user.display_name.as_deref(), Some("Riley"));
        let bad = [
            format!(r#"[{TOKEN:?}]"#),
            r#"{"token":"short","library_id":"c-1","user":{"id":7},"idle_secs":1}"#.to_string(),
            format!(r#"{{"token":"{TOKEN}","library_id":"","user":{{"id":7}},"idle_secs":1}}"#),
            format!(r#"{{"token":"{TOKEN}","library_id":"c-1","user":{{"id":0}},"idle_secs":1}}"#),
            format!(r#"{{"token":"{TOKEN}","library_id":"c-1","user":{{"id":7}},"idle_secs":0}}"#),
            format!(
                r#"{{"token":"{TOKEN}","library_id":"c-1","user":{{"id":7,"display_name":"a\nb"}},"idle_secs":1}}"#
            ),
            format!(
                r#"{{"token":"{TOKEN}","token":"{TOKEN}","library_id":"c-1","user":{{"id":7}},"idle_secs":1}}"#
            ),
            format!(
                r#"{{"token":"{TOKEN}","library_id":"c-1","user":{{"id":7}},"idle_secs":1{}}}"#,
                " ".repeat(MAX_CLERK_ANSWER_BYTES)
            ),
        ];
        for body in bad {
            assert_eq!(parse_clerk_token_answer(body.as_bytes()), None, "{body}");
        }
    }

    #[test]
    fn a_token_parses_under_its_scheme_only() {
        assert_eq!(TOKEN.len(), CLERK_TOKEN_LEN);
        let header = format!("Bookclerk-Clerk {TOKEN}");
        assert_eq!(parse_clerk_authorization(header.as_bytes()), Some(TOKEN));
        let lower = format!("bookclerk-clerk {TOKEN}");
        assert_eq!(parse_clerk_authorization(lower.as_bytes()), Some(TOKEN));
        for refused in [
            format!("Bearer {TOKEN}"),
            format!("Bookclerk-Clerk  {TOKEN}"),
            format!("Bookclerk-Clerk {TOKEN} "),
            format!("Bookclerk-Clerk {TOKEN}x"),
            format!("Bookclerk-Clerk{TOKEN}"),
            String::from("Bookclerk-Clerk "),
            String::from("Bookclerk-Clerk"),
            format!("Bookclerk-Node {TOKEN}"),
        ] {
            assert_eq!(
                parse_clerk_authorization(refused.as_bytes()),
                None,
                "{refused}"
            );
        }
        assert_eq!(parse_clerk_authorization(b"Bookclerk-Clerk \xff\xfe"), None);
    }

    #[test]
    fn tokens_are_base64url_of_fixed_length() {
        assert!(is_clerk_token(TOKEN));
        assert!(!is_clerk_token(&TOKEN[1..]));
        assert!(!is_clerk_token(&format!("{}+", &TOKEN[1..])));
        assert!(!is_clerk_token(&format!("{}=", &TOKEN[1..])));
    }

    #[test]
    fn clerk_names_are_short_and_printable() {
        assert!(is_clerk_name("Riley's laptop"));
        assert!(!is_clerk_name(""));
        assert!(!is_clerk_name("   "));
        assert!(!is_clerk_name("a\nb"));
        assert!(!is_clerk_name(&"x".repeat(MAX_CLERK_NAME_BYTES + 1)));
        assert!(is_clerk_name(&"x".repeat(MAX_CLERK_NAME_BYTES)));
    }
}

/// Reads a `POST /api/clerk/v1/token` body.
///
/// # Arguments
///
/// * `body` - The request body bytes, from another machine.
///
/// # Returns
///
/// The request when the body is one JSON object with exactly `code`,
/// `code_verifier` and `redirect_uri`, each a string written once, or
/// `None`.
#[must_use]
pub fn parse_clerk_token_request(body: &[u8]) -> Option<ClerkTokenRequest> {
    // A derived struct also reads from a JSON array; the body is an object.
    let first = body
        .iter()
        .find(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'));
    if first != Some(&b'{') {
        return None;
    }
    serde_json::from_slice(body).ok()
}

/// Largest `POST /api/clerk/v1/token` answer a clerk reads.
pub const MAX_CLERK_ANSWER_BYTES: usize = 8 * 1024;

/// Longest display name a clerk keeps from a token answer.
pub const MAX_CLERK_DISPLAY_NAME_BYTES: usize = 128;

/// Longest idle period a clerk accepts from a token answer: a year.
const MAX_CLERK_IDLE_SECS: i64 = 366 * 24 * 60 * 60;

/// Reads the library's `POST /api/clerk/v1/token` answer on the clerk.
///
/// The answer comes from another machine, so it is checked before any of
/// it is kept: at most [`MAX_CLERK_ANSWER_BYTES`] of UTF-8 (all of it,
/// unknown keys included), one JSON object with no known key written twice, `token` a session token ([`is_clerk_token`]),
/// `library_id` a library id ([`crate::is_library_id`]), `user.id` a
/// positive integer, `user.display_name` (optional) at most
/// [`MAX_CLERK_DISPLAY_NAME_BYTES`] bytes with no control characters, and
/// `idle_secs` from 1 second to a year. Keys it does not know are ignored,
/// so a newer library can add to the answer.
///
/// # Arguments
///
/// * `body` - The answer body bytes.
///
/// # Returns
///
/// The answer, or `None` when it breaks any rule.
#[must_use]
pub fn parse_clerk_token_answer(body: &[u8]) -> Option<ClerkTokenAnswer> {
    if body.len() > MAX_CLERK_ANSWER_BYTES {
        return None;
    }
    // serde skips the strings of unknown keys without checking them, so
    // the whole body is checked here.
    let text = std::str::from_utf8(body).ok()?;
    if !text
        .trim_start_matches([' ', '\t', '\n', '\r'])
        .starts_with('{')
    {
        return None;
    }
    let answer: ClerkTokenAnswer = serde_json::from_str(text).ok()?;
    let name_ok = answer.user.display_name.as_deref().is_none_or(|name| {
        name.len() <= MAX_CLERK_DISPLAY_NAME_BYTES && !name.chars().any(char::is_control)
    });
    (is_clerk_token(&answer.token)
        && crate::is_library_id(&answer.library_id)
        && answer.user.id > 0
        && name_ok
        && (1..=MAX_CLERK_IDLE_SECS).contains(&answer.idle_secs))
    .then_some(answer)
}

/// What the library stored with an approved code, as the token exchange
/// checks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredClerkCode<'a> {
    /// The OAuth client the code was approved for.
    pub client_id: &'a str,
    /// The redirect URI the authorize request named.
    pub redirect_uri: &'a str,
    /// The PKCE method the authorize request named.
    pub method: &'a str,
    /// The PKCE challenge the authorize request sent.
    pub challenge: &'a str,
}

/// True when `request` redeems `stored`: the code belongs to
/// [`CLERK_CLIENT_ID`], the redirect URI is the same, the method is S256
/// and the trimmed verifier hashes to the challenge (compared in constant
/// time).
///
/// # Arguments
///
/// * `stored` - The approved code's record.
/// * `request` - The token request.
///
/// # Returns
///
/// Whether the exchange may mint a library card.
#[must_use]
pub fn clerk_exchange_matches(stored: &StoredClerkCode<'_>, request: &ClerkTokenRequest) -> bool {
    let verifier = request.code_verifier.trim();
    let expected = crate::pkce_challenge(verifier);
    let challenge = stored.challenge.as_bytes();
    let same_challenge = expected.len() == challenge.len()
        && expected
            .as_bytes()
            .iter()
            .zip(challenge)
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0;
    !verifier.is_empty()
        && stored.client_id == CLERK_CLIENT_ID
        && stored.redirect_uri == request.redirect_uri
        && stored.method == "S256"
        && same_challenge
}
