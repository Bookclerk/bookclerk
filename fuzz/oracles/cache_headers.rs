//! Shared oracle for the `cache_headers` fuzz target and the
//! `bookclerk-cover` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! `If-None-Match` comes from whoever requests an image, and
//! [`if_none_match`] decides from it whether to answer `304`. The input's
//! first byte picks a mode:
//!
//! - **Even: built lists.** The rest, split on `0xFF`, is a tag seed then
//!   list elements. Each element's first byte says whether it is weak and
//!   whether it names the current tag; otherwise its other bytes, mapped
//!   into `etagc`, are its opaque part. The header joins the elements with
//!   `", "`, and the oracle knows the answer independently: a match exactly
//!   when some element's opaque part equals the current tag.
//! - **Odd: raw headers.** The rest is the header as sent. Whatever it is,
//!   a match other than `*` requires the quoted tag to appear in it, and
//!   the answer does not change when optional whitespace is added at the
//!   ends.

use bookclerk_cover::http_cache::{if_none_match, EntityTag, MAX_IF_NONE_MATCH_BYTES};

/// Largest input worth checking.
pub const MAX_INPUT_BYTES: usize = MAX_IF_NONE_MATCH_BYTES + 64;

/// Separates the tag seed and the list elements in built mode.
const SEPARATOR: u8 = 0xff;

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - Mode byte, then the mode's bytes.
///
/// # Panics
///
/// When the reader disagrees with the oracle.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    if mode % 2 == 0 {
        check_built(rest);
    } else {
        check_raw(rest);
    }
}

/// Built mode: a well-formed list whose answer the oracle knows.
fn check_built(rest: &[u8]) {
    let mut parts = rest.split(|&b| b == SEPARATOR);
    let tag = EntityTag::of(parts.next().unwrap_or_default());
    let mut header = Vec::new();
    let mut expected = false;
    for element in parts {
        let Some((&flags, body)) = element.split_first() else {
            continue;
        };
        let opaque: Vec<u8> = if flags & 2 == 2 {
            tag.opaque().as_bytes().to_vec()
        } else {
            body.iter().map(|&b| to_etagc(b)).collect()
        };
        expected |= opaque == tag.opaque().as_bytes();
        if !header.is_empty() {
            header.extend_from_slice(b", ");
        }
        if flags & 1 == 1 {
            header.extend_from_slice(b"W/");
        }
        header.push(b'"');
        header.extend_from_slice(&opaque);
        header.push(b'"');
    }
    if header.len() > MAX_IF_NONE_MATCH_BYTES {
        expected = false;
    }
    assert_eq!(
        if_none_match(&header, &tag),
        expected,
        "header {:?}",
        String::from_utf8_lossy(&header)
    );
}

/// Maps any byte into `etagc` (`!`, `#` to `~`, or obs-text), keeping the
/// ones already in it.
fn to_etagc(b: u8) -> u8 {
    if b == 0x21 || (0x23..=0x7e).contains(&b) || b >= 0x80 {
        b
    } else {
        b'a'
    }
}

/// Raw mode: properties that hold for any header.
fn check_raw(header: &[u8]) {
    // A tag the input is unlikely to hold, and one it may: the first 32
    // bytes, when they spell a tag.
    let tags = [EntityTag::of(header), EntityTag::of(b"")];
    for tag in &tags {
        let got = if_none_match(header, tag);
        let star = header
            .iter()
            .filter(|&&b| b != b' ' && b != b'\t')
            .eq(b"*".iter());
        if got && !star {
            let quoted = tag.header_value();
            assert!(
                header
                    .windows(quoted.len())
                    .any(|window| window == quoted.as_bytes()),
                "matched without the quoted tag in {:?}",
                String::from_utf8_lossy(header)
            );
        }
        if header.len() + 2 <= MAX_IF_NONE_MATCH_BYTES {
            let mut padded = Vec::with_capacity(header.len() + 2);
            padded.push(b' ');
            padded.extend_from_slice(header);
            padded.push(b'\t');
            assert_eq!(
                if_none_match(&padded, tag),
                got,
                "padding changed the answer"
            );
        }
    }
}
