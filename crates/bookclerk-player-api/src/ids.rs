//! The ids the `/player` API hands out and reads back.
//!
//! - A **library** is the whole library the patron may see
//!   ([`WHOLE_LIBRARY_ID`]); collections join as libraries of their own
//!   once the library has them.
//! - An **item** is an edition with a stored copy: its id is the edition
//!   id.
//! - A **file** of an item is one of its audio tracks: its `ino` is the
//!   track's place in play order, in decimal.
//! - An **author**, **series** or **narrator** is a value of the library's
//!   page index ([`facet_id`]): its key in URL-safe base64, so the same
//!   name is the same id wherever it appears, and a filter by it reads the
//!   index directly.

/// The library id of the whole library the patron may see.
pub const WHOLE_LIBRARY_ID: &str = "bookclerk-library";

/// The longest id the API reads back (an edition, author, series or
/// bookmark id), in bytes: room for a [`facet_id`] of the longest key.
pub const MAX_ID_BYTES: usize = 344;

/// The longest page index key a [`facet_id`] carries, in bytes (the page
/// index's own bound on a value).
pub const MAX_FACET_KEY_BYTES: usize = 256;

/// Most tracks an item lists; the playback core's own bound.
pub const MAX_TRACKS: usize = 512;

/// True when `id` has the shape of an id this library hands out: 1 to
/// [`MAX_ID_BYTES`] bytes of ASCII letters, digits, `-`, `_`, `.` and
/// `:`. Anything else is answered as unknown without a lookup.
#[must_use]
pub fn is_entity_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// The id of a page index value (an author, series or narrator): its key
/// (lowercase, trimmed) in URL-safe base64 without padding.
///
/// # Arguments
///
/// * `key` - The value's key, at most [`MAX_FACET_KEY_BYTES`] bytes.
#[must_use]
pub fn facet_id(key: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.as_bytes())
}

/// The page index key a [`facet_id`] names.
///
/// # Returns
///
/// `None` unless `id` is an entity id that decodes to 1 to
/// [`MAX_FACET_KEY_BYTES`] bytes of UTF-8 with no control character.
#[must_use]
pub fn parse_facet_id(id: &str) -> Option<String> {
    use base64::Engine as _;
    if !is_entity_id(id) {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(id.as_bytes())
        .ok()?;
    let key = String::from_utf8(bytes).ok()?;
    (!key.is_empty() && key.len() <= MAX_FACET_KEY_BYTES && !key.chars().any(char::is_control))
        .then_some(key)
}

/// The track a file `ino` names: its index in play order.
///
/// # Arguments
///
/// * `ino` - The path segment an app sent.
///
/// # Returns
///
/// `None` unless `ino` is a canonical decimal (no sign, no leading zero)
/// below [`MAX_TRACKS`].
#[must_use]
pub fn parse_file_ino(ino: &str) -> Option<usize> {
    if ino.is_empty() || ino.len() > 3 || !ino.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if ino.len() > 1 && ino.starts_with('0') {
        return None;
    }
    let index: usize = ino.parse().ok()?;
    (index < MAX_TRACKS).then_some(index)
}

/// The `ino` of the track at `index`.
#[must_use]
pub fn file_ino(index: usize) -> String {
    index.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_ids_are_bounded_and_plain() {
        assert!(is_entity_id("5d2c8e1a-7b3f-4a6e-9c0d-1e2f3a4b5c6d"));
        assert!(is_entity_id("ed_1:a.b"));
        assert!(!is_entity_id(""));
        assert!(!is_entity_id("../etc"));
        assert!(!is_entity_id("a b"));
        assert!(!is_entity_id(&"a".repeat(MAX_ID_BYTES + 1)));
    }

    #[test]
    fn file_inos_are_canonical_track_indexes() {
        assert_eq!(parse_file_ino("0"), Some(0));
        assert_eq!(parse_file_ino("511"), Some(511));
        for bad in ["", "512", "01", "-1", "+1", "1a", "9999"] {
            assert_eq!(parse_file_ino(bad), None, "{bad}");
        }
        assert_eq!(file_ino(7), "7");
    }

    #[test]
    fn facet_ids_round_trip_their_keys() {
        for key in [
            "ursula k. le guin",
            "the expanse",
            "é",
            &"x".repeat(MAX_FACET_KEY_BYTES),
        ] {
            let id = facet_id(key);
            assert!(is_entity_id(&id), "{key}");
            assert_eq!(parse_facet_id(&id).as_deref(), Some(key));
        }
        assert_eq!(parse_facet_id(""), None);
        assert_eq!(parse_facet_id("a b"), None);
        assert_eq!(parse_facet_id(&facet_id("a\nb")), None);
        assert_eq!(
            parse_facet_id(&facet_id(&"x".repeat(MAX_FACET_KEY_BYTES + 1))),
            None
        );
    }
}
