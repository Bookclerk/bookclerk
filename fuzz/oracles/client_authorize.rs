//! Shared oracle for the `client_authorize` fuzz target and the
//! `bookclerk-node-proto` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the authorization request a clerk sends to a
//! library's `/oidc/authorize` (`bookclerk-node-proto::parse_client_query`,
//! then `client_request_problem`): a query string from another machine
//! that decides whether the library shows its consent page and where it
//! sends the code. The invariant, checked against this file's own reading
//! of the query: it parses exactly when no parameter repeats and
//! `client_id`, `redirect_uri`, `response_type` and `code_challenge` are
//! all present, each field is the decoded value sent, and the request is
//! accepted exactly when the client id, response type, S256 challenge,
//! scope, redirect URI, client name and state all follow the rules written
//! out below.

use std::collections::HashMap;

use bookclerk_node_proto::{client_request_problem, parse_client_query};

/// Largest input worth checking; a real request is under 600 bytes.
pub const MAX_INPUT_BYTES: usize = 4096;

/// The decoded parameters, or `None` when one repeats.
fn decoded(raw: &str) -> Option<HashMap<String, String>> {
    let mut map = HashMap::new();
    for (name, value) in url::form_urlencoded::parse(raw.as_bytes()) {
        if map.insert(name.into_owned(), value.into_owned()).is_some() {
            return None;
        }
    }
    Some(map)
}

/// True for 43 characters from `A-Z a-z 0-9 - _`.
fn challenge_ok(raw: &str) -> bool {
    raw.len() == 43
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// True for an `https` URL with a host, or an `http` URL on a loopback
/// address or `localhost`, with no user info and no fragment.
fn redirect_ok(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return false;
    }
    let host = url.host_str().unwrap_or("");
    match url.scheme() {
        "https" => !host.is_empty(),
        "http" => {
            let bare = host.trim_start_matches('[').trim_end_matches(']');
            bare.eq_ignore_ascii_case("localhost")
                || bare
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }
        _ => false,
    }
}

/// True for a name of 1 to 64 bytes, not only spaces, with no control
/// characters.
fn name_ok(raw: &str) -> bool {
    !raw.trim().is_empty() && raw.len() <= 64 && !raw.chars().any(char::is_control)
}

/// Checks one input against the invariant.
///
/// # Arguments
///
/// * `data` - The raw query bytes.
///
/// # Returns
///
/// True when the request is accepted.
///
/// # Panics
///
/// When the parser or validator disagrees with the oracle.
#[must_use]
pub fn check(data: &[u8]) -> bool {
    if data.len() > MAX_INPUT_BYTES {
        return false;
    }
    let Ok(raw) = std::str::from_utf8(data) else {
        return false;
    };
    let parsed = parse_client_query(raw);
    let Some(map) = decoded(raw) else {
        assert!(parsed.is_none(), "a repeated parameter parsed: {raw:?}");
        return false;
    };
    let required = [
        "client_id",
        "redirect_uri",
        "response_type",
        "code_challenge",
    ];
    if !required.iter().all(|key| map.contains_key(*key)) {
        assert!(parsed.is_none(), "a missing parameter parsed: {raw:?}");
        return false;
    }
    let Some(params) = parsed else {
        panic!("a well-formed query did not parse: {raw:?}");
    };
    let get = |key: &str| map.get(key).cloned();
    assert_eq!(Some(params.client_id.clone()), get("client_id"));
    assert_eq!(Some(params.redirect_uri.clone()), get("redirect_uri"));
    assert_eq!(Some(params.response_type.clone()), get("response_type"));
    assert_eq!(Some(params.code_challenge.clone()), get("code_challenge"));
    assert_eq!(params.scope, get("scope"));
    assert_eq!(params.state, get("state"));
    assert_eq!(params.code_challenge_method, get("code_challenge_method"));
    assert_eq!(params.client_name, get("client_name"));

    let expected = map["client_id"] == "bookclerk-client"
        && map["response_type"] == "code"
        && get("code_challenge_method").as_deref() == Some("S256")
        && challenge_ok(&map["code_challenge"])
        && get("scope").as_deref().map(str::trim) == Some("library:member")
        && redirect_ok(&map["redirect_uri"])
        && get("client_name").as_deref().is_some_and(name_ok)
        && get("state")
            .as_deref()
            .is_none_or(|s| s.len() <= 512 && !s.chars().any(char::is_control));
    let accepted = client_request_problem(&params).is_none();
    assert_eq!(accepted, expected, "acceptance differs for {raw:?}");
    accepted
}
