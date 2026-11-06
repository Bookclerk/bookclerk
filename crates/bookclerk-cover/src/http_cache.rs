//! HTTP caching for the images the daemon serves and the ones it fetches.
//!
//! **Serving.** Every image the cover service answers with carries a strong
//! [`EntityTag`] derived from its bytes, so a browser that already holds a
//! copy can ask again with `If-None-Match` and get a `304 Not Modified`
//! instead of the body. [`if_none_match`] reads that request header.
//!
//! **Fetching.** When the covers plugin fetches an image, it passes the
//! upstream server's caching headers back to the host. [`upstream_policy`]
//! reads them into an [`UpstreamPolicy`]: whether the image may be stored
//! and for how long it stays fresh, clamped between [`FRESHNESS_FLOOR`] and
//! [`FRESHNESS_CEILING`], plus the [`Validators`] to send on the next
//! fetch so an unchanged image costs a `304`.
//!
//! Both kinds of header come from outside (any client, any image host), so
//! they are untrusted input: the readers work on byte slices with no I/O,
//! never allocate in proportion to the input, and treat anything they
//! cannot read as absent, which only ever costs a full answer or an earlier
//! refresh. The `cache_headers` fuzz target drives them against an
//! independent oracle.

use std::time::{Duration, SystemTime};

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

/// Shortest time a fetched image stays fresh, whatever upstream says: an
/// image host asking for `no-cache` or `max-age=0` is asked again after an
/// hour, not on every view.
pub const FRESHNESS_FLOOR: Duration = Duration::from_secs(60 * 60);

/// How long a fetched image stays fresh when upstream sends no `max-age`,
/// `s-maxage` or `Expires`.
pub const FRESHNESS_DEFAULT: Duration = Duration::from_secs(24 * 60 * 60);

/// Longest time a fetched image stays fresh, whatever upstream says, so a
/// changed profile picture or logo shows within a week.
pub const FRESHNESS_CEILING: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Longest upstream header value read, in bytes (the plugin ABI's
/// `maxCoverHeaderBytes`); a longer value is treated as absent.
pub const MAX_RESPONSE_HEADER_BYTES: usize = 1024;

/// The caching headers of one upstream image response, as raw bytes; `None`
/// for a header the response did not carry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpstreamHeaders<'a> {
    /// `Cache-Control`.
    pub cache_control: Option<&'a [u8]>,
    /// `Expires`.
    pub expires: Option<&'a [u8]>,
    /// `Date`.
    pub date: Option<&'a [u8]>,
    /// `Age`.
    pub age: Option<&'a [u8]>,
    /// `ETag`.
    pub etag: Option<&'a [u8]>,
    /// `Last-Modified`.
    pub last_modified: Option<&'a [u8]>,
}

/// Whether a fetched image may be kept, and for how long it is fresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    /// Upstream sent `no-store`: serve this answer, keep nothing.
    DoNotStore,
    /// Keep the image; it is fresh for this long from when it was received,
    /// always between [`FRESHNESS_FLOOR`] and [`FRESHNESS_CEILING`].
    Store {
        /// Freshness lifetime left at receipt.
        fresh_for: Duration,
    },
}

/// Validators to send when fetching the image again, so an unchanged image
/// costs a `304 Not Modified`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Validators {
    /// The upstream `ETag`, checked to be one well-formed entity tag and
    /// trimmed; sent back verbatim as `If-None-Match`.
    etag: Option<String>,
    /// The upstream `Last-Modified`, parsed; sent back as
    /// `If-Modified-Since`.
    last_modified: Option<SystemTime>,
}

impl Validators {
    /// Validators from stored values, as a cache index kept them.
    ///
    /// # Arguments
    ///
    /// * `etag` - A stored entity tag; dropped unless it is one well-formed
    ///   tag no longer than [`MAX_RESPONSE_HEADER_BYTES`].
    /// * `last_modified` - A stored modification time.
    #[must_use]
    pub fn new(etag: Option<&str>, last_modified: Option<SystemTime>) -> Self {
        Self {
            etag: etag.and_then(|value| entity_tag_value(value.as_bytes())),
            last_modified,
        }
    }

    /// The `If-None-Match` value to send, when upstream sent an `ETag`.
    #[must_use]
    pub fn if_none_match(&self) -> Option<&str> {
        self.etag.as_deref()
    }

    /// When upstream says the image last changed.
    #[must_use]
    pub fn last_modified(&self) -> Option<SystemTime> {
        self.last_modified
    }

    /// The `If-Modified-Since` value to send, when upstream sent a
    /// `Last-Modified`.
    #[must_use]
    pub fn if_modified_since(&self) -> Option<String> {
        self.last_modified.map(httpdate::fmt_http_date)
    }

    /// Whether there is anything to revalidate with.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.etag.is_none() && self.last_modified.is_none()
    }
}

/// What the host does with one fetched image, read from upstream's headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamPolicy {
    /// Whether to keep it and for how long it is fresh.
    pub storage: Storage,
    /// What to revalidate it with.
    pub validators: Validators,
}

/// Reads upstream's caching headers into the host's policy for one image.
///
/// Follows RFC 9111 for a shared cache, with Bookclerk's limits:
///
/// - `no-store` means [`Storage::DoNotStore`]. `private` is ignored: the
///   host keeps images for its own users, and a picture marked `private`
///   is still the one the user asked for.
/// - The lifetime is the first `s-maxage`, else the first `max-age`, else
///   `Expires` minus `Date` (or minus `received` when `Date` is absent or
///   unreadable; an unreadable `Expires` means already stale), else
///   [`FRESHNESS_DEFAULT`]. `no-cache` means a lifetime of zero.
/// - An explicit lifetime loses the response's age: the larger of `Age`
///   and `received` minus `Date`.
/// - The result is clamped to [`FRESHNESS_FLOOR`] ..= [`FRESHNESS_CEILING`].
///
/// A header longer than [`MAX_RESPONSE_HEADER_BYTES`] is read as absent.
///
/// # Arguments
///
/// * `headers` - Upstream's caching headers.
/// * `received` - When the host received the response.
///
/// # Returns
///
/// The storage decision and the validators.
#[must_use]
pub fn upstream_policy(headers: &UpstreamHeaders<'_>, received: SystemTime) -> UpstreamPolicy {
    let directives = CacheDirectives::parse(bounded(headers.cache_control).unwrap_or_default());
    let validators = Validators {
        etag: bounded(headers.etag).and_then(entity_tag_value),
        last_modified: bounded(headers.last_modified).and_then(http_date),
    };
    if directives.no_store {
        return UpstreamPolicy {
            storage: Storage::DoNotStore,
            validators,
        };
    }
    let date = bounded(headers.date).and_then(http_date);
    let explicit = if directives.no_cache {
        Some(Duration::ZERO)
    } else if let Some(seconds) = directives.s_maxage.or(directives.max_age) {
        Some(Duration::from_secs(seconds))
    } else {
        bounded(headers.expires).map(|raw| {
            http_date(raw).map_or(Duration::ZERO, |expires| {
                expires
                    .duration_since(date.unwrap_or(received))
                    .unwrap_or(Duration::ZERO)
            })
        })
    };
    let fresh_for = match explicit {
        None => FRESHNESS_DEFAULT,
        Some(lifetime) => {
            let age_value = bounded(headers.age)
                .and_then(delta_seconds)
                .map_or(Duration::ZERO, Duration::from_secs);
            let apparent = date
                .and_then(|date| received.duration_since(date).ok())
                .unwrap_or(Duration::ZERO);
            lifetime.saturating_sub(age_value.max(apparent))
        }
    };
    UpstreamPolicy {
        storage: Storage::Store {
            fresh_for: fresh_for.clamp(FRESHNESS_FLOOR, FRESHNESS_CEILING),
        },
        validators,
    }
}

/// `value`, unless it is longer than [`MAX_RESPONSE_HEADER_BYTES`].
fn bounded(value: Option<&[u8]>) -> Option<&[u8]> {
    value.filter(|v| v.len() <= MAX_RESPONSE_HEADER_BYTES)
}

/// The `Cache-Control` directives [`upstream_policy`] acts on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CacheDirectives {
    /// `no-store` was present.
    no_store: bool,
    /// `no-cache` was present (with or without field names).
    no_cache: bool,
    /// The first readable `max-age`, in seconds.
    max_age: Option<u64>,
    /// The first readable `s-maxage`, in seconds.
    s_maxage: Option<u64>,
}

impl CacheDirectives {
    /// Reads a `Cache-Control` value: a comma-separated list of
    /// `name[=token|"quoted"]`, names compared without case. An element
    /// that does not follow the grammar is skipped; the rest still count.
    fn parse(mut rest: &[u8]) -> Self {
        let mut out = Self::default();
        loop {
            rest = skip_separators(rest);
            if rest.is_empty() {
                return out;
            }
            let name_end = rest
                .iter()
                .position(|&b| !is_tchar(b))
                .unwrap_or(rest.len());
            let name = &rest[..name_end];
            rest = skip_ows(&rest[name_end..]);
            let mut argument: Option<&[u8]> = None;
            let mut well_formed = !name.is_empty();
            if let Some(after_eq) = rest.strip_prefix(b"=") {
                let after_eq = skip_ows(after_eq);
                if let Some(quoted) = after_eq.strip_prefix(b"\"") {
                    match quoted_string_end(quoted) {
                        Some(end) => {
                            argument = Some(&quoted[..end]);
                            rest = &quoted[end + 1..];
                        }
                        None => return out,
                    }
                } else {
                    let end = after_eq
                        .iter()
                        .position(|&b| !is_tchar(b))
                        .unwrap_or(after_eq.len());
                    argument = Some(&after_eq[..end]);
                    well_formed &= end > 0;
                    rest = &after_eq[end..];
                }
                rest = skip_ows(rest);
            }
            if !matches!(rest.first(), None | Some(b',')) {
                well_formed = false;
                let skip = rest.iter().position(|&b| b == b',').unwrap_or(rest.len());
                rest = &rest[skip..];
            }
            if well_formed {
                out.apply(name, argument);
            }
        }
    }

    /// Records one well-formed directive.
    fn apply(&mut self, name: &[u8], argument: Option<&[u8]>) {
        if name.eq_ignore_ascii_case(b"no-store") {
            self.no_store = true;
        } else if name.eq_ignore_ascii_case(b"no-cache") {
            self.no_cache = true;
        } else if name.eq_ignore_ascii_case(b"max-age") && self.max_age.is_none() {
            self.max_age = argument.and_then(delta_seconds);
        } else if name.eq_ignore_ascii_case(b"s-maxage") && self.s_maxage.is_none() {
            self.s_maxage = argument.and_then(delta_seconds);
        }
    }
}

/// Reads `delta-seconds` (one or more digits, surrounding whitespace
/// allowed). A value too large for `u64` saturates, as RFC 9111 section
/// 1.2.2 allows; the result is clamped to a week anyway.
fn delta_seconds(raw: &[u8]) -> Option<u64> {
    let digits = trim_ows(raw);
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(digits.iter().fold(0u64, |acc, &d| {
        acc.saturating_mul(10).saturating_add(u64::from(d - b'0'))
    }))
}

/// Reads an HTTP-date (IMF-fixdate or either obsolete form).
fn http_date(raw: &[u8]) -> Option<SystemTime> {
    let text = std::str::from_utf8(trim_ows(raw)).ok()?;
    httpdate::parse_http_date(text).ok()
}

/// The value as one well-formed entity tag (`"opaque"` or `W/"opaque"`),
/// trimmed, or `None`.
fn entity_tag_value(raw: &[u8]) -> Option<String> {
    let trimmed = trim_ows(raw);
    if trimmed.len() > MAX_RESPONSE_HEADER_BYTES {
        return None;
    }
    let (_, after) = read_entity_tag(trimmed)?;
    if !after.is_empty() {
        return None;
    }
    // `etagc` allows obs-text, which a header value can carry but a Rust
    // string cannot hold losslessly; such a tag is not kept.
    std::str::from_utf8(trimmed)
        .ok()
        .filter(|s| s.is_ascii())
        .map(str::to_owned)
}

/// Whether `b` is a `tchar` (a token character, RFC 9110 section 5.6.2).
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Index of the closing quote of a quoted-string whose opening quote was
/// already consumed, honouring backslash escapes.
fn quoted_string_end(input: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            b'"' => return Some(i),
            b'\\' => i += 2,
            _ => i += 1,
        }
    }
    None
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

    /// A fixed receipt time: 2026-09-01T00:00:00Z.
    fn received() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_788_220_800)
    }

    fn fresh_for(headers: &UpstreamHeaders<'_>) -> Duration {
        match upstream_policy(headers, received()).storage {
            Storage::Store { fresh_for } => fresh_for,
            Storage::DoNotStore => panic!("expected Store"),
        }
    }

    fn with_cc(value: &str) -> Duration {
        fresh_for(&UpstreamHeaders {
            cache_control: Some(value.as_bytes()),
            ..UpstreamHeaders::default()
        })
    }

    const HOUR: Duration = Duration::from_secs(3600);

    #[test]
    fn no_headers_get_the_default_lifetime() {
        assert_eq!(fresh_for(&UpstreamHeaders::default()), FRESHNESS_DEFAULT);
        assert_eq!(with_cc("public, private"), FRESHNESS_DEFAULT);
        assert_eq!(with_cc(""), FRESHNESS_DEFAULT);
    }

    #[test]
    fn max_age_and_s_maxage_are_clamped() {
        assert_eq!(with_cc("max-age=7200"), 2 * HOUR);
        assert_eq!(with_cc("MAX-AGE = \"10800\""), 3 * HOUR);
        assert_eq!(with_cc("max-age=0"), FRESHNESS_FLOOR);
        assert_eq!(with_cc("no-cache, max-age=7200"), FRESHNESS_FLOOR);
        assert_eq!(with_cc("no-cache=\"set-cookie\""), FRESHNESS_FLOOR);
        assert_eq!(
            with_cc("max-age=99999999999999999999999"),
            FRESHNESS_CEILING
        );
        assert_eq!(with_cc("max-age=7200, s-maxage=18000"), 5 * HOUR);
        // The first readable occurrence wins.
        assert_eq!(with_cc("max-age=7200, max-age=18000"), 2 * HOUR);
        assert_eq!(with_cc("max-age=x, max-age=18000"), 5 * HOUR);
        // A malformed element is skipped; the others count.
        assert_eq!(with_cc("max-age=7200 junk, foo=\"a,b\""), FRESHNESS_DEFAULT);
        assert_eq!(with_cc("ext=\"a, b\\\" c\", max-age=7200"), 2 * HOUR);
        assert_eq!(with_cc("=1, max-age=7200"), 2 * HOUR);
        assert_eq!(with_cc("max-age=, max-age=7200"), 2 * HOUR);
        // An unterminated quote ends the list.
        assert_eq!(with_cc("x=\"open, max-age=7200"), FRESHNESS_DEFAULT);
    }

    #[test]
    fn no_store_is_honoured_and_private_ignored() {
        let policy = upstream_policy(
            &UpstreamHeaders {
                cache_control: Some(b"private, No-Store"),
                etag: Some(b"\"v1\""),
                ..UpstreamHeaders::default()
            },
            received(),
        );
        assert_eq!(policy.storage, Storage::DoNotStore);
        assert_eq!(policy.validators.if_none_match(), Some("\"v1\""));
        assert_eq!(with_cc("private, max-age=7200"), 2 * HOUR);
    }

    #[test]
    fn age_and_date_shorten_an_explicit_lifetime() {
        let date = httpdate::fmt_http_date(received() - 2 * HOUR);
        let headers = |cc: &'static [u8], age: &'static [u8]| UpstreamHeaders {
            cache_control: Some(cc),
            age: Some(age),
            ..UpstreamHeaders::default()
        };
        assert_eq!(fresh_for(&headers(b"max-age=36000", b"7200")), 8 * HOUR);
        assert_eq!(fresh_for(&headers(b"max-age=36000", b"x")), 10 * HOUR);
        // The default lifetime ignores the age.
        assert_eq!(fresh_for(&headers(b"public", b"36000")), FRESHNESS_DEFAULT);
        let dated = UpstreamHeaders {
            cache_control: Some(b"max-age=36000"),
            date: Some(date.as_bytes()),
            age: Some(b"60"),
            ..UpstreamHeaders::default()
        };
        assert_eq!(fresh_for(&dated), 8 * HOUR);
    }

    #[test]
    fn expires_counts_from_date_or_receipt() {
        let at = |offset: Duration| httpdate::fmt_http_date(received() + offset);
        let expires = at(5 * HOUR);
        assert_eq!(
            fresh_for(&UpstreamHeaders {
                expires: Some(expires.as_bytes()),
                ..UpstreamHeaders::default()
            }),
            5 * HOUR
        );
        let date = at(2 * HOUR);
        assert_eq!(
            fresh_for(&UpstreamHeaders {
                expires: Some(expires.as_bytes()),
                date: Some(date.as_bytes()),
                ..UpstreamHeaders::default()
            }),
            3 * HOUR
        );
        // An unreadable or past `Expires` is stale.
        for raw in [&b"0"[..], b"-1", at(Duration::ZERO).as_bytes()] {
            assert_eq!(
                fresh_for(&UpstreamHeaders {
                    expires: Some(raw),
                    ..UpstreamHeaders::default()
                }),
                FRESHNESS_FLOOR
            );
        }
        // `max-age` beats `Expires`.
        assert_eq!(
            fresh_for(&UpstreamHeaders {
                cache_control: Some(b"max-age=36000"),
                expires: Some(b"0"),
                ..UpstreamHeaders::default()
            }),
            10 * HOUR
        );
    }

    #[test]
    fn validators_keep_only_well_formed_values() {
        let read = |etag: &'static [u8], modified: &'static [u8]| {
            upstream_policy(
                &UpstreamHeaders {
                    etag: Some(etag),
                    last_modified: Some(modified),
                    ..UpstreamHeaders::default()
                },
                received(),
            )
            .validators
        };
        let good = read(b" W/\"abc\" ", b"Tue, 01 Sep 2026 00:00:00 GMT");
        assert_eq!(good.if_none_match(), Some("W/\"abc\""));
        assert_eq!(good.last_modified(), Some(received()));
        assert_eq!(
            good.if_modified_since().as_deref(),
            Some("Tue, 01 Sep 2026 00:00:00 GMT")
        );
        assert!(!good.is_empty());
        for bad in [&b"abc"[..], b"\"a\", \"b\"", b"\"a", b"\"\xff\""] {
            assert_eq!(read(bad, b"").if_none_match(), None);
        }
        assert!(read(b"", b"yesterday").is_empty());
        let long = vec![b'a'; MAX_RESPONSE_HEADER_BYTES + 1];
        assert_eq!(
            upstream_policy(
                &UpstreamHeaders {
                    cache_control: Some(&long),
                    ..UpstreamHeaders::default()
                },
                received()
            )
            .storage,
            Storage::Store {
                fresh_for: FRESHNESS_DEFAULT
            }
        );
        let stored = Validators::new(Some("\"x\""), Some(received()));
        assert_eq!(stored.if_none_match(), Some("\"x\""));
        assert_eq!(Validators::new(Some("x"), None), Validators::default());
    }
}
