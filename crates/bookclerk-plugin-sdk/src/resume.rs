//! Sans-IO decisions for resuming an interrupted HTTP download.
//!
//! Audience: guests that download large files (store audio, archives).
//! `crate::download` (feature `http`) drives these functions over the
//! SDK's HTTP client; they are separate and IO-free so the fuzz target
//! `download_resume` and the unit tests exercise exactly the code a guest
//! runs.
//!
//! A download keeps the bytes it already has and asks the server for the
//! rest with `Range: bytes=<offset>-`. The server's answer is untrusted:
//! [`parse_content_range`] reads its `Content-Range` header and
//! [`next_step`] decides whether the body continues the partial file,
//! replaces it, finishes it, or ends the download. A body is only ever
//! appended where it provably starts at the partial's end, so a server
//! that answers a different range, changes the file's size, or ignores
//! the range never splices unrelated bytes into the file.

/// Longest `Content-Range` value read, in bytes. A valid one for a file
/// under 2^64 bytes is at most `bytes ` plus three 20-digit numbers.
pub const MAX_CONTENT_RANGE_BYTES: usize = 80;

/// Longest validator (`ETag` or `Last-Modified` value) kept for `If-Range`,
/// in bytes. Real ones are well under this; a longer value is not used.
pub const MAX_VALIDATOR_BYTES: usize = 256;

/// A `Content-Range` header the server sent, as [`parse_content_range`]
/// reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentRange {
    /// `bytes <first>-<last>/<total>`: the body holds bytes `first` to
    /// `last` (inclusive) of a file `total` bytes long, or of a file whose
    /// length the server did not state (`/*`).
    Bytes {
        /// Offset of the body's first byte in the file.
        first: u64,
        /// Offset of the body's last byte in the file; at least `first`.
        last: u64,
        /// The whole file's length in bytes, greater than `last`; `None`
        /// for `/*`.
        total: Option<u64>,
    },
    /// `bytes */<total>`: the range asked for is past the end of a file
    /// `total` bytes long (the form a `416` carries).
    Unsatisfied {
        /// The whole file's length in bytes.
        total: u64,
    },
}

/// Parses a `Content-Range` value (RFC 9110 §14.4) for the `bytes` unit.
///
/// Accepts exactly `bytes <first>-<last>/<total|*>` or `bytes */<total>`,
/// with one space after the unit and decimal numbers only. Optional
/// whitespace around the value is ignored; anything else (another unit,
/// signs, a range whose last byte is before its first or at or past the
/// stated length, numbers that overflow 64 bits, a value longer than
/// [`MAX_CONTENT_RANGE_BYTES`]) is refused.
///
/// # Arguments
///
/// * `value` - The header value as received, not necessarily UTF-8.
///
/// # Returns
///
/// The parsed range, or `None` when the value is not a valid `bytes`
/// content range.
#[must_use]
pub fn parse_content_range(value: &[u8]) -> Option<ContentRange> {
    let value = trim_ows(value);
    if value.len() > MAX_CONTENT_RANGE_BYTES {
        return None;
    }
    let rest = value.strip_prefix(b"bytes ")?;
    let slash = rest.iter().position(|&b| b == b'/')?;
    let (range, total) = (&rest[..slash], &rest[slash + 1..]);
    if range == b"*" {
        return Some(ContentRange::Unsatisfied {
            total: parse_decimal(total)?,
        });
    }
    let dash = range.iter().position(|&b| b == b'-')?;
    let first = parse_decimal(&range[..dash])?;
    let last = parse_decimal(&range[dash + 1..])?;
    if last < first {
        return None;
    }
    let total = if total == b"*" {
        None
    } else {
        let total = parse_decimal(total)?;
        if last >= total {
            return None;
        }
        Some(total)
    };
    Some(ContentRange::Bytes { first, last, total })
}

/// Strips HTTP optional whitespace (space and tab) from both ends.
fn trim_ows(mut value: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = value {
        value = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = value {
        value = rest;
    }
    value
}

/// Parses one or more ASCII digits into a `u64`, refusing overflow.
fn parse_decimal(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0u64, |acc, &b| {
        if !b.is_ascii_digit() {
            return None;
        }
        acc.checked_mul(10)?.checked_add(u64::from(b - b'0'))
    })
}

/// What a download already has when it reads a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partial {
    /// Bytes already written, from the start of the file; the request
    /// asked for `Range: bytes=<offset>-` when this is above zero.
    pub offset: u64,
    /// The file's length as an earlier response stated it, if any; a
    /// response stating another length means the file changed.
    pub total: Option<u64>,
}

/// The parts of a response's head [`next_step`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseHead<'a> {
    /// HTTP status code.
    pub status: u16,
    /// The raw `Content-Range` value, when the header was sent.
    pub content_range: Option<&'a [u8]>,
    /// `Content-Length`, when the header was sent and is a number.
    pub content_length: Option<u64>,
}

/// What to do with a response, as [`next_step`] decides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The body continues the partial at its current end (`206` starting
    /// exactly at the offset): append it.
    Append {
        /// The file's length, when the server stated it.
        total: Option<u64>,
        /// Offset just past the body's last byte, for a `206` (its
        /// range's last byte plus one): a body that runs past it does not
        /// match its own range, and one that stops there leaves the rest
        /// of the file to ask for. `None` for a `200`, whose body runs to
        /// the end of the file.
        end: Option<u64>,
    },
    /// The body is the whole file from byte 0 (`200`, or a `206` from 0
    /// asked without a range): drop what the partial holds and write the
    /// body in its place.
    Replace {
        /// The file's length, when the server stated it.
        total: Option<u64>,
        /// Offset just past the body's last byte, for a `206` (its
        /// range's last byte plus one): a body that runs past it does not
        /// match its own range, and one that stops there leaves the rest
        /// of the file to ask for. `None` for a `200`, whose body runs to
        /// the end of the file.
        end: Option<u64>,
    },
    /// The partial already holds the whole file (`416` stating a length
    /// equal to the offset): nothing more to read.
    Complete,
    /// The answer does not fit the partial (another range, a changed
    /// length, an unsatisfiable range at another length): drop the
    /// partial and ask again for the whole file.
    StartOver,
    /// A transient refusal (`408`, `429`, `5xx`): wait and ask again for
    /// the same range.
    Retry,
    /// Any other status: the download fails with it.
    Fail,
}

/// Decides what a response means for a partial download.
///
/// Only a `206` whose `Content-Range` starts exactly at `partial.offset`
/// and states the same length as `partial.total` (when both are known),
/// with a `Content-Length` (when sent) matching the range, is appended. A
/// `200` replaces the partial. A `416` finishes the download only when it
/// states `bytes */<offset>` for a non-empty partial and agrees with the
/// known length. Everything else that names a range the partial cannot
/// use starts over; `408`, `429` and `5xx` are retried; any other status
/// fails.
///
/// # Arguments
///
/// * `partial` - What the download held when it sent the request.
/// * `head` - The response's status and range headers.
///
/// # Returns
///
/// The [`Step`] to take.
#[must_use]
pub fn next_step(partial: Partial, head: ResponseHead<'_>) -> Step {
    match head.status {
        200 => Step::Replace {
            total: head.content_length,
            end: None,
        },
        206 => {
            let Some(ContentRange::Bytes { first, last, total }) =
                head.content_range.and_then(parse_content_range)
            else {
                return Step::StartOver;
            };
            if head
                .content_length
                .is_some_and(|len| Some(len) != (last - first).checked_add(1))
            {
                return Step::StartOver;
            }
            if let (Some(known), Some(stated)) = (partial.total, total) {
                if known != stated {
                    return Step::StartOver;
                }
            }
            if first != partial.offset {
                return Step::StartOver;
            }
            let Some(end) = last.checked_add(1) else {
                return Step::StartOver;
            };
            let total = total.or(partial.total);
            if total.is_some_and(|total| end > total) {
                return Step::StartOver;
            }
            let end = Some(end);
            if partial.offset == 0 {
                Step::Replace { total, end }
            } else {
                Step::Append { total, end }
            }
        }
        416 => match head.content_range.and_then(parse_content_range) {
            Some(ContentRange::Unsatisfied { total })
                if partial.offset > 0
                    && total == partial.offset
                    && partial.total.is_none_or(|known| known == total) =>
            {
                Step::Complete
            }
            Some(ContentRange::Unsatisfied { .. } | ContentRange::Bytes { .. }) | None => {
                Step::StartOver
            }
        },
        408 | 429 | 500..=599 => Step::Retry,
        _ => Step::Fail,
    }
}

/// A validator that identifies one version of the remote file, sent as
/// `If-Range` so a server whose file changed answers the whole new file
/// instead of a range of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Validator {
    /// A strong `ETag` (never one starting with `W/`, which `If-Range`
    /// may not carry).
    ETag(String),
    /// A `Last-Modified` date, as sent.
    LastModified(String),
}

impl Validator {
    /// Picks the validator to resume with from a response's headers.
    ///
    /// A strong `ETag` wins over `Last-Modified`. A value is used only
    /// when it is visible ASCII (and spaces, for a date) of at most
    /// [`MAX_VALIDATOR_BYTES`] bytes, so it can be sent back unchanged.
    ///
    /// # Arguments
    ///
    /// * `etag` - The raw `ETag` value, when sent.
    /// * `last_modified` - The raw `Last-Modified` value, when sent.
    ///
    /// # Returns
    ///
    /// The validator, or `None` when neither header is usable.
    #[must_use]
    pub fn from_headers(etag: Option<&[u8]>, last_modified: Option<&[u8]>) -> Option<Self> {
        let usable = |raw: &[u8], spaces: bool| {
            let raw = trim_ows(raw);
            (!raw.is_empty()
                && raw.len() <= MAX_VALIDATOR_BYTES
                && raw
                    .iter()
                    .all(|&b| b.is_ascii_graphic() || (spaces && b == b' ')))
            .then(|| String::from_utf8_lossy(raw).into_owned())
        };
        if let Some(tag) = etag.and_then(|raw| usable(raw, false)) {
            if tag.starts_with('"') && tag.ends_with('"') && tag.len() >= 2 {
                return Some(Self::ETag(tag));
            }
        }
        last_modified
            .and_then(|raw| usable(raw, true))
            .map(Self::LastModified)
    }

    /// The value to send as `If-Range`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::ETag(value) | Self::LastModified(value) => value,
        }
    }
}

#[cfg(test)]
// Test code: a failed assertion is the test failing, not a documented panic.
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    /// A response head with the given status and headers.
    fn head(
        status: u16,
        range: Option<&'static str>,
        length: Option<u64>,
    ) -> ResponseHead<'static> {
        ResponseHead {
            status,
            content_range: range.map(str::as_bytes),
            content_length: length,
        }
    }

    #[test]
    fn parses_ranges_and_refuses_malformed_ones() {
        assert_eq!(
            parse_content_range(b"bytes 0-9/10"),
            Some(ContentRange::Bytes {
                first: 0,
                last: 9,
                total: Some(10)
            })
        );
        assert_eq!(
            parse_content_range(b" bytes 5-9/* "),
            Some(ContentRange::Bytes {
                first: 5,
                last: 9,
                total: None
            })
        );
        assert_eq!(
            parse_content_range(b"bytes */42"),
            Some(ContentRange::Unsatisfied { total: 42 })
        );
        for bad in [
            &b"bytes 9-5/10"[..],
            b"bytes 0-10/10",
            b"items 0-1/2",
            b"bytes  0-1/2",
            b"bytes 0-1",
            b"bytes -1-2/3",
            b"bytes +1-2/3",
            b"bytes 0-1/",
            b"bytes */*",
            b"bytes 0-99999999999999999999/1",
            b"",
        ] {
            assert_eq!(parse_content_range(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn appends_only_where_the_partial_ends() {
        let partial = Partial {
            offset: 100,
            total: Some(1000),
        };
        assert_eq!(
            next_step(partial, head(206, Some("bytes 100-999/1000"), Some(900))),
            Step::Append {
                total: Some(1000),
                end: Some(1000)
            }
        );
        // Another start, a changed length, or a length that disagrees
        // with the range never appends.
        for (range, length) in [
            ("bytes 0-999/1000", Some(1000)),
            ("bytes 101-999/1000", Some(899)),
            ("bytes 100-1099/1100", Some(1000)),
            ("bytes 100-999/1000", Some(10)),
        ] {
            assert_eq!(
                next_step(partial, head(206, Some(range), length)),
                Step::StartOver,
                "{range}"
            );
        }
        assert_eq!(next_step(partial, head(206, None, None)), Step::StartOver);
    }

    #[test]
    fn a_full_answer_replaces_the_partial() {
        let partial = Partial {
            offset: 100,
            total: None,
        };
        assert_eq!(
            next_step(partial, head(200, None, Some(5))),
            Step::Replace {
                total: Some(5),
                end: None
            }
        );
        let fresh = Partial {
            offset: 0,
            total: None,
        };
        assert_eq!(
            next_step(fresh, head(206, Some("bytes 0-4/5"), None)),
            Step::Replace {
                total: Some(5),
                end: Some(5)
            }
        );
    }

    #[test]
    fn unsatisfiable_finishes_only_at_the_stated_length() {
        let partial = Partial {
            offset: 10,
            total: None,
        };
        assert_eq!(
            next_step(partial, head(416, Some("bytes */10"), None)),
            Step::Complete
        );
        assert_eq!(
            next_step(partial, head(416, Some("bytes */9"), None)),
            Step::StartOver
        );
        assert_eq!(next_step(partial, head(416, None, None)), Step::StartOver);
        let known = Partial {
            offset: 10,
            total: Some(11),
        };
        assert_eq!(
            next_step(known, head(416, Some("bytes */10"), None)),
            Step::StartOver
        );
    }

    #[test]
    fn transient_statuses_retry_and_others_fail() {
        let partial = Partial {
            offset: 0,
            total: None,
        };
        for status in [408, 429, 500, 503, 599] {
            assert_eq!(next_step(partial, head(status, None, None)), Step::Retry);
        }
        for status in [301, 401, 403, 404, 410] {
            assert_eq!(next_step(partial, head(status, None, None)), Step::Fail);
        }
    }

    #[test]
    fn validators_prefer_strong_etags() {
        assert_eq!(
            Validator::from_headers(Some(b"\"abc\""), Some(b"Wed, 21 Oct 2015 07:28:00 GMT")),
            Some(Validator::ETag("\"abc\"".into()))
        );
        assert_eq!(
            Validator::from_headers(Some(b"W/\"abc\""), Some(b"Wed, 21 Oct 2015 07:28:00 GMT")),
            Some(Validator::LastModified(
                "Wed, 21 Oct 2015 07:28:00 GMT".into()
            ))
        );
        assert_eq!(Validator::from_headers(Some(b"\"a\nb\""), None), None);
        assert_eq!(Validator::from_headers(None, None), None);
    }
}
