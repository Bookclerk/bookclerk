//! Shared oracle for the `copy_range` fuzz target and the
//! `bookclerk-node-proto` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The `Range` header of a copy download (`GET
//! /api/copies/v1/{copy_id}/files/{n}`) comes from whoever holds a copy
//! token, and [`parse_copy_range`] decides from it which bytes of a
//! library's file to read. The input is the file's size (the first eight
//! bytes, big endian; missing bytes are zero) followed by the header as
//! sent.
//!
//! Whatever the header is:
//!
//! - a part lies inside the file: at least one byte, starting before the
//!   end and ending at or before it, so the read never passes the file;
//! - an unsatisfiable answer and a part are given only to a header that
//!   is `bytes=` followed by digits and exactly one `-`;
//! - the answer equals the one worked out here by a second reading of the
//!   header (digits folded into a wide integer rather than parsed), so the
//!   parser and the oracle would have to be wrong the same way.

use bookclerk_node_proto::{parse_copy_range, CopyRange};

/// Largest input worth checking: the size and a header far longer than
/// any range the parser reads.
pub const MAX_INPUT_BYTES: usize = 8 + 256;

/// A run of 1 to 19 ASCII digits as a wide integer; `None` for anything
/// else.
fn digits(text: &[u8]) -> Option<u128> {
    if text.is_empty() || text.len() > 19 {
        return None;
    }
    text.iter().try_fold(0u128, |sum, byte| {
        byte.is_ascii_digit()
            .then(|| sum * 10 + u128::from(byte - b'0'))
    })
}

/// The answer the header should get, read a second way.
fn expected(header: &[u8], size: u64) -> CopyRange {
    let size = u128::from(size);
    let Some(spec) = header.strip_prefix(b"bytes=") else {
        return CopyRange::Whole;
    };
    let Some(dash) = spec.iter().position(|byte| *byte == b'-') else {
        return CopyRange::Whole;
    };
    let (first, last) = (&spec[..dash], &spec[dash + 1..]);
    let part = |offset: u128, length: u128| CopyRange::Part {
        offset: u64::try_from(offset).unwrap_or(u64::MAX),
        length: u64::try_from(length).unwrap_or(u64::MAX),
    };
    match (first.is_empty(), last.is_empty()) {
        (true, true) => CopyRange::Whole,
        (true, false) => match digits(last) {
            None => CopyRange::Whole,
            Some(suffix) if suffix == 0 || size == 0 => CopyRange::Unsatisfiable,
            Some(suffix) => {
                let length = suffix.min(size);
                part(size - length, length)
            }
        },
        (false, open) => {
            let Some(start) = digits(first) else {
                return CopyRange::Whole;
            };
            let end = if open {
                size.saturating_sub(1)
            } else {
                match digits(last) {
                    Some(end) if end >= start => end.min(size.saturating_sub(1)),
                    _ => return CopyRange::Whole,
                }
            };
            if start >= size {
                CopyRange::Unsatisfiable
            } else {
                part(start, end - start + 1)
            }
        }
    }
}

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - Eight bytes of file size, then the header.
///
/// # Returns
///
/// True when the header named a part of the file.
///
/// # Panics
///
/// When the parser breaks an invariant or disagrees with the oracle.
pub fn check(data: &[u8]) -> bool {
    if data.len() > MAX_INPUT_BYTES {
        return false;
    }
    let mut size = [0u8; 8];
    let head = data.len().min(8);
    size[..head].copy_from_slice(&data[..head]);
    let size = u64::from_be_bytes(size);
    let header = &data[head..];
    let Ok(text) = std::str::from_utf8(header) else {
        return false;
    };
    let answer = parse_copy_range(text, size);
    if let CopyRange::Part { offset, length } = answer {
        assert!(length >= 1, "an empty part for {text:?} of {size}");
        assert!(offset < size, "a part past the end for {text:?} of {size}");
        assert!(
            offset.checked_add(length).is_some_and(|end| end <= size),
            "a part running past the end for {text:?} of {size}"
        );
    }
    if answer != CopyRange::Whole {
        let spec = header
            .strip_prefix(b"bytes=")
            .unwrap_or_else(|| panic!("{text:?} was read as a range without the bytes unit"));
        assert!(
            spec.iter().filter(|byte| **byte == b'-').count() == 1
                && spec
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || *byte == b'-'),
            "{text:?} was read as a range"
        );
    }
    assert_eq!(answer, expected(header, size), "{text:?} of {size}");
    matches!(answer, CopyRange::Part { .. })
}
