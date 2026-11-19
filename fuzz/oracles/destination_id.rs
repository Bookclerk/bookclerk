//! Shared oracle for the `destination_id` fuzz target and the
//! `bookclerk-config` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! A destination id reaches [`DestinationId`] from another node: an acquire
//! plan's `PlanDestination.id` and upload records name the owner's
//! destinations by it, and the host stores it in `edition_files`. The input
//! is the id as received (any bytes, read as UTF-8 lossily so every input
//! is tried). The invariants, checked against an independent byte-by-byte
//! reading:
//!
//! - [`DestinationId::new`] accepts exactly the ids of 1 to
//!   [`DestinationId::MAX_LEN`] bytes, each a lowercase ASCII letter, a
//!   digit, `_` or `-`, so no path separator, dot, space or control byte
//!   ever reaches a storage key or a table row;
//! - an accepted id keeps its text unchanged, prints as it, and survives a
//!   JSON round trip; a refused id is refused by JSON parsing too.

use bookclerk_config::DestinationId;

/// Largest input worth checking: a little past the longest id.
pub const MAX_INPUT_BYTES: usize = 256;

/// True when `id` is a destination id by a reading independent of the
/// parser: its length in range and every byte in the allowed set.
fn oracle_accepts(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return false;
    }
    bytes
        .iter()
        .all(|&b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - The id as received.
///
/// # Panics
///
/// When the parser disagrees with the oracle.
// Oracle code: a disagreement is the finding, reported by panicking.
#[allow(clippy::expect_used, clippy::panic)]
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let text = String::from_utf8_lossy(data).into_owned();
    assert_eq!(
        DestinationId::MAX_LEN,
        64,
        "oracle and parser disagree on the length limit"
    );
    let expected = oracle_accepts(&text);
    let json = serde_json::to_string(&text).expect("a string always serializes");
    let from_json = serde_json::from_str::<DestinationId>(&json);
    match DestinationId::new(text.clone()) {
        Ok(id) => {
            assert!(expected, "accepted {text:?}, which is not a destination id");
            assert_eq!(id.as_str(), text);
            assert_eq!(id.to_string(), text);
            let round = from_json.expect("an accepted id parses from JSON");
            assert_eq!(round, id);
            assert_eq!(serde_json::to_string(&id).expect("serialize"), json);
        }
        Err(_) => {
            assert!(!expected, "refused {text:?}, which is a destination id");
            assert!(from_json.is_err(), "JSON accepted {text:?}");
        }
    }
}
