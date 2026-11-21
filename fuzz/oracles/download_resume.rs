//! Shared oracle for the `download_resume` fuzz target and the
//! `bookclerk-plugin-sdk` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is what a guest knows when a store's CDN answers a resumed
//! download, and the answer's head:
//!
//! - byte 0: flags (bit 0: the file's length is already known; bit 1: the
//!   answer sent `Content-Length`; bit 2: it sent `Content-Range`);
//! - bytes 1..9: the resume offset (big endian);
//! - bytes 9..17: the known length; bytes 17..25: `Content-Length`;
//! - bytes 25..27: the status;
//! - the rest: the `Content-Range` value as sent.
//!
//! The invariants, checked against a second reading of the header (UTF-8
//! text split on its separators, digits folded into a 128-bit integer):
//!
//! - `parse_content_range` gives the same answer as the oracle's parser;
//! - `next_step` gives the step the rules in `bookclerk_plugin_sdk::resume`
//!   state, so bytes are appended only where a `206` provably starts at
//!   the offset with an agreeing length (and its body is held to the
//!   range's end), and a `416` finishes only at a stated length equal to
//!   a non-empty offset.

use bookclerk_plugin_sdk::resume::{
    next_step, parse_content_range, ContentRange, Partial, ResponseHead, Step,
    MAX_CONTENT_RANGE_BYTES,
};

/// Bytes before the header value.
const HEAD_BYTES: usize = 27;

/// Reads eight big-endian bytes at `at`.
fn u64_at(data: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&data[at..at + 8]);
    u64::from_be_bytes(word)
}

/// A decimal number of at least one ASCII digit that fits in 64 bits.
fn number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut wide: u128 = 0;
    for b in text.bytes() {
        wide = wide * 10 + u128::from(b - b'0');
        if wide > u128::from(u64::MAX) {
            return None;
        }
    }
    u64::try_from(wide).ok()
}

/// The oracle's reading of a `Content-Range` value.
fn expected_range(value: &[u8]) -> Option<ContentRange> {
    let text = std::str::from_utf8(value).ok()?;
    let text = text.trim_matches(|c| c == ' ' || c == '\t');
    if text.len() > MAX_CONTENT_RANGE_BYTES {
        return None;
    }
    let rest = text.strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    if range == "*" {
        return number(total).map(|total| ContentRange::Unsatisfied { total });
    }
    let (first, last) = range.split_once('-')?;
    let (first, last) = (number(first)?, number(last)?);
    if last < first {
        return None;
    }
    if total == "*" {
        return Some(ContentRange::Bytes {
            first,
            last,
            total: None,
        });
    }
    let total = number(total)?;
    (last < total).then_some(ContentRange::Bytes {
        first,
        last,
        total: Some(total),
    })
}

/// The step the rules give.
fn expected_step(
    partial: Partial,
    status: u16,
    range: Option<ContentRange>,
    length: Option<u64>,
) -> Step {
    match status {
        200 => Step::Replace {
            total: length,
            end: None,
        },
        206 => match range {
            Some(ContentRange::Bytes { first, last, total }) => {
                let span = u128::from(last) - u128::from(first) + 1;
                let length_ok = length.is_none_or(|len| u128::from(len) == span);
                let total_ok = match (partial.total, total) {
                    (Some(a), Some(b)) => a == b,
                    _ => true,
                };
                let end = u128::from(last) + 1;
                let known = total.or(partial.total);
                let end_ok =
                    end <= u128::from(u64::MAX) && known.is_none_or(|t| end <= u128::from(t));
                if !length_ok || !total_ok || !end_ok || first != partial.offset {
                    Step::StartOver
                } else {
                    let end = u64::try_from(end).ok();
                    if partial.offset == 0 {
                        Step::Replace { total: known, end }
                    } else {
                        Step::Append { total: known, end }
                    }
                }
            }
            _ => Step::StartOver,
        },
        416 => match range {
            Some(ContentRange::Unsatisfied { total })
                if partial.offset > 0
                    && total == partial.offset
                    && partial.total.is_none_or(|t| t == total) =>
            {
                Step::Complete
            }
            _ => Step::StartOver,
        },
        408 | 429 => Step::Retry,
        s if (500..=599).contains(&s) => Step::Retry,
        _ => Step::Fail,
    }
}

/// Checks every invariant above for one input.
///
/// # Returns
///
/// The step taken, or `None` when the input is too short to decode.
///
/// # Panics
///
/// Panics when the parser or the decision breaks an invariant.
pub fn check(data: &[u8]) -> Option<Step> {
    if data.len() < HEAD_BYTES {
        return None;
    }
    let flags = data[0];
    let partial = Partial {
        offset: u64_at(data, 1),
        total: (flags & 1 != 0).then(|| u64_at(data, 9)),
    };
    let length = (flags & 2 != 0).then(|| u64_at(data, 17));
    let status = u16::from_be_bytes([data[25], data[26]]);
    let value = &data[HEAD_BYTES..];
    let sent = flags & 4 != 0;

    let parsed = parse_content_range(value);
    assert_eq!(parsed, expected_range(value), "parse of {value:?}");

    let step = next_step(
        partial,
        ResponseHead {
            status,
            content_range: sent.then_some(value),
            content_length: length,
        },
    );
    let range = if sent { parsed } else { None };
    assert_eq!(
        step,
        expected_step(partial, status, range, length),
        "{partial:?} {status} {range:?} {length:?}"
    );
    if let Step::Append { end, .. } = step {
        assert!(
            matches!(range, Some(ContentRange::Bytes { first, last, .. })
                if first == partial.offset && first > 0 && end == last.checked_add(1))
        );
    }
    Some(step)
}
