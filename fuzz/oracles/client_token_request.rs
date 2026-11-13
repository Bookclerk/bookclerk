//! Shared oracle for the `client_token_request` fuzz target and the
//! `bookclerk-node-proto` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The boundary is the body of `POST /api/client/v1/token`, which redeems a
//! library card code from another machine
//! (`bookclerk-node-proto::parse_client_token_request`), and the check
//! that the request matches the stored code (`client_exchange_matches`).
//! The first input byte picks the stored code: the clerk's own (any
//! value but 1 or 2), the branch client's (1), or one sent with the
//! `plain` method (2). The rest is the body.
//!
//! The invariant, checked against this file's own reading of the bytes as
//! an untyped JSON value: the body parses exactly when it is one object
//! with no repeated keys and exactly the string keys `code`,
//! `code_verifier` and `redirect_uri`, each field is the value sent, and
//! the exchange matches exactly when the stored code is the clerk's,
//! the redirect URI is the stored one byte for byte, and the trimmed
//! verifier is not empty and its SHA-256, base64url without padding, is
//! the stored challenge.

use base64::Engine as _;
use bookclerk_node_proto::{client_exchange_matches, parse_client_token_request, StoredClientCode};
use serde::de::{Deserializer as _, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;
use sha2::Digest as _;

/// Largest input worth checking; a real body is under 300 bytes.
pub const MAX_INPUT_BYTES: usize = 2048;

/// The redirect URI every stored code names.
pub const REDIRECT: &str = "http://127.0.0.1:41923/client/callback";

/// The challenge every stored code holds: RFC 7636's S256 example, for the
/// verifier `dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk`, written out
/// rather than computed by the code under test.
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

/// The clerk's built-in id, written out.
const CLIENT: &str = "bookclerk-client";

/// Checks one input against the invariant.
///
/// # Arguments
///
/// * `data` - The stored-code selector byte, then the body.
///
/// # Returns
///
/// True when the exchange matched.
///
/// # Panics
///
/// When the parser or the exchange check disagrees with the oracle.
#[must_use]
pub fn check(data: &[u8]) -> bool {
    if data.len() > MAX_INPUT_BYTES {
        return false;
    }
    let Some((&pick, body)) = data.split_first() else {
        return false;
    };
    let (client_id, method) = match pick {
        1 => ("bookclerk-branch", "S256"),
        2 => (CLIENT, "plain"),
        _ => (CLIENT, "S256"),
    };
    let parsed = parse_client_token_request(body);
    let Some(fields) = expected_fields(body) else {
        assert!(
            parsed.is_none(),
            "a malformed body parsed: {:?}",
            String::from_utf8_lossy(body)
        );
        return false;
    };
    let Some(request) = parsed else {
        panic!(
            "a well-formed body did not parse: {:?}",
            String::from_utf8_lossy(body)
        );
    };
    assert_eq!(request.code, fields[0]);
    assert_eq!(request.code_verifier, fields[1]);
    assert_eq!(request.redirect_uri, fields[2]);

    let stored = StoredClientCode {
        client_id,
        redirect_uri: REDIRECT,
        method,
        challenge: CHALLENGE,
    };
    let verifier = fields[1].trim();
    let hashed = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    let expected = client_id == CLIENT
        && method == "S256"
        && fields[2] == REDIRECT
        && !verifier.is_empty()
        && hashed == CHALLENGE;
    let matched = client_exchange_matches(&stored, &request);
    assert_eq!(
        matched,
        expected,
        "exchange differs for {:?}",
        String::from_utf8_lossy(data)
    );
    matched
}

/// `code`, `code_verifier` and `redirect_uri` when the body follows the
/// rules, else `None`.
fn expected_fields(body: &[u8]) -> Option<[String; 3]> {
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(body) else {
        return None;
    };
    if fields.len() != 3 || has_repeated_top_level_key(body) {
        return None;
    }
    let get = |key: &str| match fields.get(key) {
        Some(Value::String(value)) => Some(value.clone()),
        _ => None,
    };
    Some([get("code")?, get("code_verifier")?, get("redirect_uri")?])
}

/// True when the top-level JSON object in `data` writes a key twice.
/// `data` already parsed as an object.
fn has_repeated_top_level_key(data: &[u8]) -> bool {
    /// Collects an object's keys in the order written.
    struct Keys;

    impl<'de> Visitor<'de> for Keys {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an object")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Vec<String>, A::Error> {
            let mut keys = Vec::new();
            while let Some((key, IgnoredAny)) = map.next_entry::<String, IgnoredAny>()? {
                keys.push(key);
            }
            Ok(keys)
        }
    }

    let mut de = serde_json::Deserializer::from_slice(data);
    let Ok(mut keys) = de.deserialize_map(Keys) else {
        return false;
    };
    let written = keys.len();
    keys.sort();
    keys.dedup();
    keys.len() != written
}
