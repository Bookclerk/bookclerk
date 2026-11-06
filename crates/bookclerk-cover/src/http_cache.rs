//! HTTP cache validators for the images the daemon serves.
//!
//! Every image the cover service answers with carries a strong
//! [`EntityTag`] derived from its bytes, so a browser that already holds a
//! copy can ask again with `If-None-Match` and get a `304 Not Modified`
//! instead of the body. [`if_none_match`] reads that request header.
//!
//! The header comes from whoever sends the request, so it is untrusted
//! input: the reader works on a byte slice with no I/O, never allocates in
//! proportion to the input, and treats anything it cannot read as "no
//! match", which only ever costs a full `200` answer. The `cache_headers`
//! fuzz target drives it against an independent oracle.

use sha2::{Digest, Sha256};

/// Hex digits of the SHA-256 kept in an [`EntityTag`]: 128 bits, plenty to
/// tell two images apart.
const TAG_HEX_DIGITS: usize = 32;

/// Longest `If-None-Match` header read, in bytes. A browser sends one tag;
/// a longer header is treated as no match rather than scanned.
pub const MAX_IF_NONE_MATCH_BYTES: usize = 8 * 1024;

/// A strong entity tag naming one exact image body.
///
/// The opaque part is the first 32 lowercase hex digits of the body's
/// SHA-256, so equal bodies always get equal tags and the tag never needs
/// storing beside the image.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntityTag {
    /// The opaque tag: [`TAG_HEX_DIGITS`] lowercase hex digits, without the
    /// surrounding quotes.
    opaque: String,
}

impl EntityTag {
    /// The tag for `body`.
    ///
    /// # Arguments
    ///
    /// * `body` - The exact bytes the response carries.
    #[must_use]
    pub fn of(body: &[u8]) -> Self {
        let digest = Sha256::digest(body);
        let mut opaque = String::with_capacity(TAG_HEX_DIGITS);
        for byte in digest.iter().take(TAG_HEX_DIGITS / 2) {
            opaque.push(hex_digit(byte >> 4));
            opaque.push(hex_digit(byte & 0x0f));
        }
        Self { opaque }
    }

    /// The opaque part, without quotes.
    #[must_use]
    pub fn opaque(&self) -> &str {
        &self.opaque
    }

    /// The `ETag` header value: the opaque part in double quotes.
    #[must_use]
    pub fn header_value(&self) -> String {
        format!("\"{}\"", self.opaque)
    }
}

/// The lowercase hex digit for a value below 16.
fn hex_digit(nibble: u8) -> char {
    char::from(b"0123456789abcdef"[usize::from(nibble & 0x0f)])
}

/// Whether an `If-None-Match` header matches `tag`, so the answer may be
/// `304 Not Modified`.
///
/// Follows RFC 9110 section 13.1.2: the header is `*` or a comma-separated
/// list of entity tags, each `"opaque"` or weak `W/"opaque"`, compared with
/// the weak comparison function (the `W/` prefix is ignored). Optional
/// whitespace may surround each element and empty elements are skipped. A
/// header that does not follow this grammar, or is longer than
/// [`MAX_IF_NONE_MATCH_BYTES`], matches nothing.
///
/// # Arguments
///
/// * `header` - The raw header value; several header lines joined with
///   `,` read the same as one.
/// * `tag` - The current tag of the representation.
///
/// # Returns
///
/// `true` when the client's copy is current.
#[must_use]
pub fn if_none_match(header: &[u8], tag: &EntityTag) -> bool {
    if header.len() > MAX_IF_NONE_MATCH_BYTES {
        return false;
    }
    let trimmed = trim_ows(header);
    if trimmed == b"*" {
        return true;
    }
    let want = tag.opaque.as_bytes();
    let mut rest = header;
    let mut matched = false;
    loop {
        rest = skip_separators(rest);
        if rest.is_empty() {
            return matched;
        }
        let Some((opaque, after)) = read_entity_tag(rest) else {
            return false;
        };
        matched |= opaque == want;
        rest = skip_ows(after);
        match rest.first() {
            None => return matched,
            Some(b',') => {}
            Some(_) => return false,
        }
    }
}

/// Reads one entity tag at the start of `input`.
///
/// # Returns
///
/// The opaque part (between the quotes) and what follows the closing
/// quote, or `None` when `input` does not start with a well-formed tag.
fn read_entity_tag(input: &[u8]) -> Option<(&[u8], &[u8])> {
    let input = input.strip_prefix(b"W/").unwrap_or(input);
    let body = input.strip_prefix(b"\"")?;
    let end = body.iter().position(|&b| b == b'"')?;
    let opaque = &body[..end];
    if !opaque.iter().all(|&b| is_etagc(b)) {
        return None;
    }
    Some((opaque, &body[end + 1..]))
}

/// Whether `b` may appear inside an entity tag's quotes (`etagc`: `!`,
/// `#` to `~`, or obs-text).
fn is_etagc(b: u8) -> bool {
    b == 0x21 || (0x23..=0x7e).contains(&b) || b >= 0x80
}

/// `input` without leading spaces, tabs and commas (empty list elements).
fn skip_separators(input: &[u8]) -> &[u8] {
    let start = input
        .iter()
        .position(|&b| !matches!(b, b' ' | b'\t' | b','))
        .unwrap_or(input.len());
    &input[start..]
}

/// `input` without leading optional whitespace.
fn skip_ows(input: &[u8]) -> &[u8] {
    let start = input
        .iter()
        .position(|&b| !matches!(b, b' ' | b'\t'))
        .unwrap_or(input.len());
    &input[start..]
}

/// `input` without optional whitespace at either end.
fn trim_ows(input: &[u8]) -> &[u8] {
    let input = skip_ows(input);
    let end = input
        .iter()
        .rposition(|&b| !matches!(b, b' ' | b'\t'))
        .map_or(0, |i| i + 1);
    &input[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag() -> EntityTag {
        EntityTag::of(b"jpeg bytes")
    }

    #[test]
    fn a_tag_is_32_hex_digits_of_the_sha256() {
        let tag = tag();
        assert_eq!(tag.opaque().len(), 32);
        assert!(tag.opaque().bytes().all(|b| b.is_ascii_hexdigit()));
        let hex: String = Sha256::digest(b"jpeg bytes")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(tag.opaque(), &hex[..32]);
        assert_eq!(tag.header_value(), format!("\"{}\"", tag.opaque()));
        assert_ne!(EntityTag::of(b"other"), tag);
    }

    #[test]
    fn the_current_tag_matches_strong_weak_and_in_a_list() {
        let tag = tag();
        let quoted = tag.header_value();
        assert!(if_none_match(quoted.as_bytes(), &tag));
        assert!(if_none_match(format!("W/{quoted}").as_bytes(), &tag));
        assert!(if_none_match(
            format!("\"old\", {quoted} ,\"older\"").as_bytes(),
            &tag
        ));
        assert!(if_none_match(format!(" ,, {quoted},").as_bytes(), &tag));
        assert!(if_none_match(b" * ", &tag));
    }

    #[test]
    fn other_tags_and_malformed_headers_do_not_match() {
        let tag = tag();
        let opaque = tag.opaque().to_owned();
        assert!(!if_none_match(b"", &tag));
        assert!(!if_none_match(b"\"other\"", &tag));
        // Unquoted, unterminated, or with junk after a tag.
        assert!(!if_none_match(opaque.as_bytes(), &tag));
        assert!(!if_none_match(format!("\"{opaque}").as_bytes(), &tag));
        assert!(!if_none_match(format!("\"{opaque}\"x").as_bytes(), &tag));
        // A malformed element spoils a list that also holds the tag.
        assert!(!if_none_match(
            format!("\"{opaque}\", nonsense").as_bytes(),
            &tag
        ));
        // A space is not an etagc byte.
        assert!(!if_none_match(format!("\"{opaque} \"").as_bytes(), &tag));
        // `*` only on its own.
        assert!(!if_none_match(b"*, \"x\"", &tag));
        assert!(!if_none_match(b"w/\"x\"", &tag));
    }

    #[test]
    fn an_oversized_header_matches_nothing() {
        let tag = tag();
        let mut header = tag.header_value().into_bytes();
        header.resize(MAX_IF_NONE_MATCH_BYTES + 1, b' ');
        assert!(!if_none_match(&header, &tag));
    }
}
