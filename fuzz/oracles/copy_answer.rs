//! Shared oracle for the `copy_answer` fuzz target and the
//! `bookclerk-node-proto` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The head of the answer to a copy download (`GET
//! /api/copies/v1/{copy_id}/files/{n}`) comes from the library a reader
//! took the copy from, and [`parse_copy_answer`] decides from it how many
//! bytes the reader takes and where in the file it writes them. The input
//! is the byte the reader asked to resume from and the file's size (eight
//! bytes each, big endian; missing bytes are zero), then the answer as
//! received: the head, the blank line that ends it and any first bytes of
//! the body. [`copy_answer_head_end`] finds the head in it, as the reader
//! does after each read.
//!
//! Whatever the answer is:
//!
//! - the head ends where a second scan (one byte at a time) finds the
//!   first blank line, is refused when that is past the longest head, and
//!   is found the same in every shorter run of the bytes that holds the
//!   blank line and in none that does not, so where the library's writes
//!   were cut into reads never moves a byte between the head and the file;
//! - a body ends exactly at the file's end and starts at byte 0 or at the
//!   byte asked for, so the reader never writes past the size the grant
//!   stated or leaves a gap;
//! - a body from a byte above 0 is given only to a `206`;
//! - the answer equals the one worked out here by a second reading of the
//!   head (bytes compared one by one, digits folded into a wide integer),
//!   so the parser and the oracle would have to be wrong the same way.

use bookclerk_node_proto::{
    copy_answer_head_end, parse_copy_answer, CopyAnswer, MAX_COPY_ANSWER_HEAD_BYTES,
};

/// Largest input worth checking: the two numbers and an answer just past
/// the longest head the parser takes.
pub const MAX_INPUT_BYTES: usize = 16 + MAX_COPY_ANSWER_HEAD_BYTES + 64;

/// What the oracle found the head to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Read {
    /// File bytes follow.
    Body,
    /// A status without file bytes.
    Status,
    /// The head is refused.
    Refused,
}

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

/// `text` cut at each CRLF.
fn lines(text: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut at = 0;
    while at + 1 < text.len() {
        if text[at] == b'\r' && text[at + 1] == b'\n' {
            out.push(&text[start..at]);
            at += 2;
            start = at;
        } else {
            at += 1;
        }
    }
    out.push(&text[start..]);
    out
}

/// `value` without the spaces and tabs around it.
fn trimmed(mut value: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = value {
        value = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = value {
        value = rest;
    }
    value
}

/// The status of a status line, when it is one.
fn status_of(line: &[u8]) -> Option<u128> {
    let rest = line.strip_prefix(b"HTTP/1.")?;
    let (minor, rest) = rest.split_first()?;
    if !matches!(minor, b'0' | b'1') {
        return None;
    }
    let rest = rest.strip_prefix(b" ")?;
    if rest.len() < 3 || !(rest.len() == 3 || rest[3] == b' ') {
        return None;
    }
    digits(&rest[..3]).filter(|code| (100..=599).contains(code))
}

/// The `first-last/total` of a `Content-Range` value, when it states one.
fn range_of(value: &[u8]) -> Option<(u128, u128, u128)> {
    let spec = value.strip_prefix(b"bytes ")?;
    let slash = spec.iter().position(|byte| *byte == b'/')?;
    let (span, total) = (&spec[..slash], &spec[slash + 1..]);
    let dash = span.iter().position(|byte| *byte == b'-')?;
    Some((
        digits(&span[..dash])?,
        digits(&span[dash + 1..])?,
        digits(total)?,
    ))
}

/// True for a byte an HTTP header name may hold, read off the list in
/// RFC 9110 rather than by character class.
fn token_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'0'..=b'9'
            | b'a'..=b'z'
            | b'A'..=b'Z'
            | b'!'
            | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~'
    )
}

/// True for the `Content-Range` of an unsatisfied range, `bytes */<size>`.
fn unsatisfied(value: &[u8]) -> bool {
    value
        .strip_prefix(b"bytes */")
        .is_some_and(|total| digits(total).is_some())
}

/// Where the head ends in `received`, scanned one byte at a time:
/// `Ok(Some(n))` for a head of `n` bytes, `Ok(None)` while the blank line
/// may still arrive, `Err(())` when the head is too long.
fn head_end(received: &[u8]) -> Result<Option<usize>, ()> {
    let mut run = 0usize;
    for (at, byte) in received.iter().enumerate() {
        run = match (run, byte) {
            (0 | 2, b'\r') => run + 1,
            (1, b'\n') => 2,
            (3, b'\n') => {
                let head = at - 3;
                return if head <= MAX_COPY_ANSWER_HEAD_BYTES {
                    Ok(Some(head))
                } else {
                    Err(())
                };
            }
            (_, b'\r') => 1,
            _ => 0,
        };
    }
    if received.len() >= MAX_COPY_ANSWER_HEAD_BYTES + 4 {
        Err(())
    } else {
        Ok(None)
    }
}

/// The answer the head should get, read a second way; `None` when it
/// should be refused.
fn expected(head: &[u8], asked_from: u64, size: u64) -> Option<CopyAnswer> {
    if head.len() > MAX_COPY_ANSWER_HEAD_BYTES {
        return None;
    }
    if head.iter().any(|byte| {
        !(*byte == b'\t' || *byte == b'\r' || *byte == b'\n' || (0x20..=0x7e).contains(byte))
    }) {
        return None;
    }
    let lines = lines(head);
    if lines
        .iter()
        .any(|line| line.contains(&b'\r') || line.contains(&b'\n'))
    {
        return None;
    }
    let status = status_of(lines[0])?;
    let (mut lengths, mut ranges) = (Vec::new(), Vec::new());
    for line in &lines[1..] {
        let colon = line.iter().position(|byte| *byte == b':')?;
        let name = line[..colon].to_ascii_lowercase();
        if name.is_empty() || !name.iter().all(|byte| token_byte(*byte)) {
            return None;
        }
        let value = trimmed(&line[colon + 1..]);
        match name.as_slice() {
            b"transfer-encoding" => return None,
            b"content-length" => lengths.push(digits(value)?),
            b"content-range" => ranges.push(value),
            _ => {}
        }
    }
    if lengths.len() > 1 || ranges.len() > 1 {
        return None;
    }
    let length = lengths.first().copied();
    let (from, size_wide) = (u128::from(asked_from), u128::from(size));
    match status {
        200 => (ranges.is_empty() && length == Some(size_wide)).then_some(CopyAnswer::Body {
            offset: 0,
            length: size,
        }),
        206 => {
            if from == 0 || from >= size_wide {
                return None;
            }
            let (first, last, total) = range_of(ranges.first()?)?;
            (first == from
                && last + 1 == size_wide
                && total == size_wide
                && length == Some(size_wide - from))
            .then_some(CopyAnswer::Body {
                offset: asked_from,
                length: size - asked_from,
            })
        }
        416 if !ranges.iter().all(|value| unsatisfied(value)) => None,
        other => Some(CopyAnswer::Status(u16::try_from(other).unwrap_or(u16::MAX))),
    }
}

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - Eight bytes of the byte asked for, eight of the file's size,
///   then the answer as received.
///
/// # Returns
///
/// What the answer was read as; `Refused` also when its head has no end
/// yet or is too long.
///
/// # Panics
///
/// When the parser breaks an invariant or disagrees with the oracle.
pub fn check(data: &[u8]) -> Read {
    if data.len() > MAX_INPUT_BYTES {
        return Read::Refused;
    }
    let mut numbers = [0u8; 16];
    let prefix = data.len().min(16);
    numbers[..prefix].copy_from_slice(&data[..prefix]);
    let mut half = [0u8; 8];
    half.copy_from_slice(&numbers[..8]);
    let asked_from = u64::from_be_bytes(half);
    half.copy_from_slice(&numbers[8..]);
    let size = u64::from_be_bytes(half);
    let received = &data[prefix..];

    let found = copy_answer_head_end(received).map_err(|_| ());
    assert_eq!(found, head_end(received), "the head's end");
    // However the bytes were cut into reads, the same head is found: a
    // shorter run finds it exactly when it holds the blank line.
    let cuts = [
        received.len() / 2,
        received.len().saturating_sub(1),
        usize::from(numbers[7]) % (received.len() + 1),
    ];
    let around = found.ok().flatten().map(|head| [head + 3, head + 4]);
    for cut in cuts.into_iter().chain(around.into_iter().flatten()) {
        let Some(shorter) = received.get(..cut) else {
            continue;
        };
        let early = copy_answer_head_end(shorter).map_err(|_| ());
        match found {
            Ok(Some(head)) if cut >= head + 4 => assert_eq!(early, Ok(Some(head))),
            Ok(_) => assert_eq!(early, Ok(None), "a head found before its end"),
            Err(()) => assert_ne!(
                early.map(|end| end.is_some()),
                Ok(true),
                "a head found in an answer that is refused"
            ),
        }
    }
    let Ok(Some(head_len)) = found else {
        return Read::Refused;
    };
    let head = &received[..head_len];
    let shown = String::from_utf8_lossy(head);

    let answer = parse_copy_answer(head, asked_from, size).ok();
    if let Some(CopyAnswer::Body { offset, length }) = answer {
        assert_eq!(
            offset.checked_add(length),
            Some(size),
            "a body that does not end at the file's end for {shown:?}"
        );
        assert!(
            offset == 0 || offset == asked_from,
            "a body from a byte nobody asked for for {shown:?}"
        );
        if offset > 0 {
            assert!(
                head.starts_with(b"HTTP/1.1 206") || head.starts_with(b"HTTP/1.0 206"),
                "a part answered by another status for {shown:?}"
            );
        }
    }
    assert_eq!(
        answer,
        expected(head, asked_from, size),
        "{shown:?} from {asked_from} of {size}"
    );
    match answer {
        Some(CopyAnswer::Body { .. }) => Read::Body,
        Some(CopyAnswer::Status(_)) => Read::Status,
        None => Read::Refused,
    }
}
