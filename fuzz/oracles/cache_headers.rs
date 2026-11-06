//! Shared oracle for the `cache_headers` fuzz target and the
//! `bookclerk-cover` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! `If-None-Match` comes from whoever requests an image, and
//! [`if_none_match`] decides from it whether to answer `304`. An image
//! host's caching headers come back through the covers plugin, and
//! [`upstream_policy`] decides from them whether and how long to keep the
//! image. The input's first byte, modulo 4, picks a mode:
//!
//! - **0: built lists.** The rest, split on `0xFF`, is a tag seed then
//!   list elements. Each element's first byte says whether it is weak and
//!   whether it names the current tag; otherwise its other bytes, mapped
//!   into `etagc`, are its opaque part. The header joins the elements with
//!   `", "`, and the oracle knows the answer independently: a match exactly
//!   when some element's opaque part equals the current tag.
//! - **1: raw headers.** The rest is the header as sent. Whatever it is,
//!   a match other than `*` requires the quoted tag to appear in it, and
//!   the answer does not change when optional whitespace is added at the
//!   ends.
//! - **2: built `Cache-Control`.** The rest drives a list of directives
//!   (`no-store`, `no-cache`, `max-age`, `s-maxage`, `private`, unknown
//!   ones, in random case and spacing) plus an `Age`, and the oracle works
//!   out the storage decision on its own.
//! - **3: raw upstream headers.** The rest, split on `0xFF`, is
//!   `Cache-Control`, `Expires`, `Date`, `Age`, `ETag` and `Last-Modified`.
//!   Whatever they are, a kept image is fresh for a time inside the floor
//!   and ceiling, `no-store` appears in `Cache-Control` whenever the image
//!   is not kept, and a kept `ETag` is the trimmed header.

use std::time::{Duration, SystemTime};

use bookclerk_cover::http_cache::{
    if_none_match, upstream_policy, EntityTag, Storage, UpstreamHeaders, FRESHNESS_CEILING,
    FRESHNESS_DEFAULT, FRESHNESS_FLOOR, MAX_IF_NONE_MATCH_BYTES,
};

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
    match mode % 4 {
        0 => check_built(rest),
        1 => check_raw(rest),
        2 => check_built_freshness(rest),
        _ => check_raw_upstream(rest),
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

/// When the oracle says the response arrived: 2026-09-01T00:00:00Z.
fn received() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_788_220_800)
}

/// Built `Cache-Control` mode: directives from the input, answer known.
///
/// Each 3-byte chunk is one directive: the first byte picks the kind and
/// its spelling, the other two a seconds value (scaled so both sides of
/// the floor and ceiling come up). A leftover byte is the `Age` in minutes.
fn check_built_freshness(rest: &[u8]) {
    let mut header = String::new();
    let (mut no_store, mut no_cache) = (false, false);
    let (mut max_age, mut s_maxage): (Option<u64>, Option<u64>) = (None, None);
    let (chunks, remainder) = rest.as_chunks::<3>();
    for &[kind, a, b] in chunks {
        let seconds = u64::from(u16::from_be_bytes([a, b])) * 16;
        let spaced = kind & 0x80 == 0x80;
        let upper = kind & 0x40 == 0x40;
        let directive = match kind % 7 {
            0 => {
                no_store = true;
                "no-store".to_owned()
            }
            1 => {
                no_cache = true;
                "no-cache".to_owned()
            }
            2 => {
                max_age.get_or_insert(seconds);
                if spaced {
                    format!("max-age = \"{seconds}\"")
                } else {
                    format!("max-age={seconds}")
                }
            }
            3 => {
                s_maxage.get_or_insert(seconds);
                format!("s-maxage={seconds}")
            }
            4 => "private".to_owned(),
            5 => format!("x-ext=\"a, {a}\""),
            _ => "public".to_owned(),
        };
        let directive = if upper {
            directive.to_ascii_uppercase()
        } else {
            directive
        };
        if !header.is_empty() {
            header.push_str(if spaced { " ,  " } else { "," });
        }
        header.push_str(&directive);
    }
    let age = remainder.first().map(|&m| u64::from(m) * 60);
    let age_text = age.map(|a| a.to_string());
    let headers = UpstreamHeaders {
        cache_control: Some(header.as_bytes()),
        age: age_text.as_deref().map(str::as_bytes),
        ..UpstreamHeaders::default()
    };
    let expected = if no_store {
        Storage::DoNotStore
    } else {
        let lifetime = if no_cache {
            Some(0)
        } else {
            s_maxage.or(max_age)
        };
        let fresh_for = match lifetime {
            None => FRESHNESS_DEFAULT,
            Some(seconds) => Duration::from_secs(seconds.saturating_sub(age.unwrap_or(0))),
        };
        Storage::Store {
            fresh_for: fresh_for.clamp(FRESHNESS_FLOOR, FRESHNESS_CEILING),
        }
    };
    if header.len() > bookclerk_cover::http_cache::MAX_RESPONSE_HEADER_BYTES {
        return;
    }
    assert_eq!(
        upstream_policy(&headers, received()).storage,
        expected,
        "Cache-Control {header:?}, Age {age:?}"
    );
}

/// Raw upstream mode: properties that hold for any headers.
fn check_raw_upstream(rest: &[u8]) {
    let mut fields = rest.split(|&b| b == SEPARATOR).map(Some);
    let mut next = || fields.next().flatten();
    let headers = UpstreamHeaders {
        cache_control: next(),
        expires: next(),
        date: next(),
        age: next(),
        etag: next(),
        last_modified: next(),
    };
    let policy = upstream_policy(&headers, received());
    match policy.storage {
        Storage::Store { fresh_for } => {
            assert!((FRESHNESS_FLOOR..=FRESHNESS_CEILING).contains(&fresh_for));
        }
        Storage::DoNotStore => {
            let cc = headers
                .cache_control
                .unwrap_or_default()
                .to_ascii_lowercase();
            assert!(
                cc.windows(8).any(|w| w == b"no-store"),
                "not stored without no-store in {:?}",
                String::from_utf8_lossy(&cc)
            );
        }
    }
    if let Some(etag) = policy.validators.if_none_match() {
        let raw = headers.etag.unwrap_or_default();
        let trimmed: &[u8] = {
            let start = raw
                .iter()
                .position(|&b| b != b' ' && b != b'\t')
                .unwrap_or(raw.len());
            let end = raw
                .iter()
                .rposition(|&b| b != b' ' && b != b'\t')
                .map_or(start, |i| i + 1);
            &raw[start..end.max(start)]
        };
        assert_eq!(etag.as_bytes(), trimmed);
        assert!(etag.ends_with('"'));
    }
    assert_eq!(
        upstream_policy(&headers, received()),
        policy,
        "not deterministic"
    );
}
