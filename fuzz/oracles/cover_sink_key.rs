//! Shared oracle for the `cover_sink_key` fuzz target and the
//! `bookclerk-plugin-abi` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! A cover fetcher guest names the slot each image body goes into by a
//! string key. The input's first byte is the request's URL count, the rest
//! is the key. The invariant, checked against an independent byte-by-byte
//! reading of the key: [`cover_sink_slot`] answers `Some(i)` exactly when the
//! key is the canonical decimal spelling of an `i` below the URL count, and
//! that spelling round-trips (`i.to_string() == key`), so no two keys name
//! one slot.

use bookclerk_plugin_abi::cover_sink_slot;

/// Largest input worth checking.
pub const MAX_INPUT_BYTES: usize = 64;

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - URL count byte, then the key bytes.
///
/// # Panics
///
/// When the parser disagrees with the oracle.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((&count, key)) = data.split_first() else {
        return;
    };
    let Ok(key) = std::str::from_utf8(key) else {
        return;
    };
    let slots = usize::from(count);
    let got = cover_sink_slot(key, slots);
    assert_eq!(got, expected(key, slots), "key {key:?} with {slots} slots");
    if let Some(index) = got {
        assert_eq!(index.to_string(), key, "slot {index} has another spelling");
    }
    // Every slot's canonical key opens exactly that slot.
    for index in 0..slots.min(40) {
        assert_eq!(cover_sink_slot(&index.to_string(), slots), Some(index));
    }
    assert_eq!(cover_sink_slot(&slots.to_string(), slots), None);
}

/// The slot `key` names, read digit by digit without `str::parse`.
fn expected(key: &str, slots: usize) -> Option<usize> {
    let bytes = key.as_bytes();
    if bytes.is_empty() || (bytes.len() > 1 && bytes[0] == b'0') {
        return None;
    }
    let mut value: usize = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(usize::from(b - b'0'))?;
        if value >= slots {
            return None;
        }
    }
    Some(value)
}
