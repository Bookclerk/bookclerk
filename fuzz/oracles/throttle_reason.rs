//! Shared oracle for the `throttle_reason` fuzz target and the
//! `bookclerk-storefront` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is a store's refusal text, as the Audible guest reads it from
//! a refused license request; it is read as UTF-8 with invalid bytes
//! replaced. A refusal classified as a throttle pauses the whole account,
//! so the classification is checked, not only that it does not panic.
//!
//! The invariants, restated here byte by byte rather than through the
//! crate's lowercase-and-search:
//!
//! - the text is a throttle exactly when one of the throttle words below
//!   appears in it, ignoring ASCII case;
//! - flipping the text's ASCII case never changes the answer;
//! - adding "Too Many Requests" to any text makes it a throttle;
//! - adding other limit wording ("device limit reached", "download limit")
//!   to a text that is not a throttle keeps it not a throttle.

use bookclerk_storefront::is_throttle_reason;

/// The words that make a refusal a throttle, as the throttle spec states
/// them (lowercase ASCII).
const THROTTLE_WORDS: [&[u8]; 6] = [
    b"throttl",
    b"rate limit",
    b"rate-limit",
    b"ratelimit",
    b"too many",
    b"429",
];

/// Wording about limits that are not throttling.
const OTHER_LIMITS: [&str; 3] = [
    " device limit reached",
    " download limit",
    " Limit exceeded for this title",
];

/// Whether `text` contains `word`, comparing ASCII letters without case.
fn contains_ignoring_case(text: &[u8], word: &[u8]) -> bool {
    text.windows(word.len())
        .any(|window| window.eq_ignore_ascii_case(word))
}

/// The answer the spec gives for `text`.
fn expected(text: &str) -> bool {
    THROTTLE_WORDS
        .iter()
        .any(|word| contains_ignoring_case(text.as_bytes(), word))
}

/// Checks every invariant above for one input.
///
/// # Panics
///
/// Panics when the classifier breaks an invariant.
pub fn check(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let throttled = is_throttle_reason(&text);
    assert_eq!(
        throttled,
        expected(&text),
        "classification differs from the throttle words for {text:?}"
    );
    assert_eq!(
        is_throttle_reason(&text.to_ascii_uppercase()),
        throttled,
        "upper-casing changed the answer for {text:?}"
    );
    assert_eq!(
        is_throttle_reason(&text.to_ascii_lowercase()),
        throttled,
        "lower-casing changed the answer for {text:?}"
    );
    assert!(
        is_throttle_reason(&format!("{text} Too Many Requests")),
        "an added throttle phrase was missed after {text:?}"
    );
    if !throttled {
        for other in OTHER_LIMITS {
            assert!(
                !is_throttle_reason(&format!("{text}{other}")),
                "{other:?} after {text:?} was taken for a throttle"
            );
        }
    }
}
