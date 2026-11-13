//! Shared oracle for the `client_authorization` fuzz target and the
//! `bookclerk-node-proto` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the `Authorization` header value a client app sends with
//! its library card (`bookclerk-node-proto::parse_client_authorization`):
//! bytes from another machine that decide whether a request is signed in.
//! The invariant, checked against this file's own byte-by-byte reading
//! rather than the parser's string handling: a value yields a token exactly
//! when it is the scheme `Bookclerk-Client` in any ASCII case, one space,
//! and 43 characters from `A-Z a-z 0-9 - _`, with nothing else. The token
//! is then exactly those 43 bytes.

use bookclerk_node_proto::parse_client_authorization;

/// Largest input worth checking; a real value is 60 bytes.
pub const MAX_INPUT_BYTES: usize = 256;

/// The scheme, written out rather than read from the parser.
const SCHEME: &[u8] = b"bookclerk-client";

/// The token length, written out rather than read from the parser.
const TOKEN_LEN: usize = 43;

/// The token the oracle expects in `data`, or `None`.
fn expected(data: &[u8]) -> Option<&[u8]> {
    if data.len() != SCHEME.len() + 1 + TOKEN_LEN {
        return None;
    }
    let (scheme, rest) = data.split_at(SCHEME.len());
    for (have, want) in scheme.iter().zip(SCHEME) {
        if have.to_ascii_lowercase() != *want {
            return None;
        }
    }
    let (space, token) = rest.split_at(1);
    if space != b" " {
        return None;
    }
    for b in token {
        let ok = matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_');
        if !ok {
            return None;
        }
    }
    Some(token)
}

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - A header value.
///
/// # Returns
///
/// True when the value yields a token.
///
/// # Panics
///
/// When the parser and the oracle disagree.
pub fn check(data: &[u8]) -> bool {
    if data.len() > MAX_INPUT_BYTES {
        return false;
    }
    let parsed = parse_client_authorization(data);
    let want = expected(data);
    assert_eq!(
        parsed.map(str::as_bytes),
        want,
        "parse_client_authorization disagrees with the oracle on {data:?}"
    );
    parsed.is_some()
}
