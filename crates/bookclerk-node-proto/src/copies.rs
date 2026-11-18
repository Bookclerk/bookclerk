//! Taking a copy of an edition from the library that already holds it.
//!
//! A reader's page of the library's books says which editions the library
//! holds as processed audio ([`crate::WindowCopy`]). To take one, the
//! reader asks for it and downloads its files:
//!
//! 1. reader → library: [`crate::Body::CopyGrant`] naming the edition by
//!    its store listing (store, marketplace, product id). The frame names
//!    no person: the person is the asking node's owner.
//! 2. library → reader: [`crate::Body::CopyGranted`] with a copy id, a
//!    bearer token sealed to the node's box key ([`crate::seal_copy_token`]),
//!    what the copy is, and its files; or `error` with `refused` (the
//!    library does not trust the asker) or `not_found` (the person cannot
//!    take that edition, or the library holds no copy of it: one code for
//!    both, so the answer does not say which editions exist).
//! 3. the reader fetches each file with `GET` [`COPIES_PATH`]
//!    `<copy_id>/files/<n>` (`n` is the file's index in the answer),
//!    sending `Authorization:` [`COPY_AUTH_SCHEME`] ` <token>`. `Range` is
//!    honoured, so a broken download resumes.
//!
//! The library checks the token, the asker's standing and the person's
//! right to the edition again on every download, and the token ends after
//! [`CopyGrantedFrame::expires_in_secs`] seconds.

use serde::{Deserialize, Serialize};

use crate::catalog::{is_marketplace, is_product_id, is_store_id};
use crate::frames::{is_storage_key, UploadProbe};
use crate::window::WindowCopy;

/// Path prefix of the download endpoint: the files of copy `<copy_id>`
/// are at `COPIES_PATH<copy_id>/files/<n>`.
pub const COPIES_PATH: &str = "/api/copies/v1/";

/// `Authorization` scheme of the download endpoint.
pub const COPY_AUTH_SCHEME: &str = "Bookclerk-Copy";

/// Most files one [`crate::Body::CopyGranted`] may list.
pub const MAX_COPY_FILES: usize = 1000;

/// Longest [`CopyFile::name`], in bytes.
pub const MAX_COPY_NAME_BYTES: usize = 512;

/// Longest a copy token may be stated to live, in seconds (one day).
pub const MAX_COPY_TOKEN_SECS: u64 = 24 * 60 * 60;

/// Largest [`CopyFile::size`] (1 TiB): above any audiobook file.
pub const MAX_COPY_FILE_BYTES: u64 = 1 << 40;

/// What a file of a copy is, as the library recorded it when it wrote the
/// file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CopyRole {
    /// The edition's primary audio file.
    Audio,
    /// One part of a multi-part audio edition.
    Part,
    /// One chapter file.
    Chapter,
    /// The companion PDF.
    Pdf,
    /// The cover image.
    Cover,
    /// A metadata sidecar.
    Sidecar,
    /// A transcript.
    Transcript,
    /// The ebook.
    Epub,
}

/// One file of a copy ([`crate::Body::CopyGranted`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyFile {
    /// The file's path below the title's folder on the library, `/`
    /// separated ([`crate::is_storage_key`], at most
    /// [`MAX_COPY_NAME_BYTES`]). The reader places it under its own
    /// naming, so this is a hint to what the file is, never a path to
    /// write to as given.
    pub name: String,
    /// What the file is.
    pub role: CopyRole,
    /// Exact size in bytes, at most [`MAX_COPY_FILE_BYTES`].
    pub size: u64,
    /// The media type the library's probe found, for a probed audio file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// What the library's probe found in an audio file; absent for other
    /// files and for audio it has not probed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<UploadProbe>,
}

impl CopyFile {
    /// True when the name is a relative path within its length limit, the
    /// size is within [`MAX_COPY_FILE_BYTES`], the media type (if stated)
    /// is a media type token and the probe (if stated) is well formed.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        is_storage_key(&self.name)
            && self.name.len() <= MAX_COPY_NAME_BYTES
            && self.size <= MAX_COPY_FILE_BYTES
            && self
                .content_type
                .as_deref()
                .is_none_or(crate::window::is_media_type)
            && self.probe.as_ref().is_none_or(UploadProbe::is_well_formed)
    }
}

/// True for a copy id: a lowercase hyphenated UUID.
#[must_use]
pub fn is_copy_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(at, b)| match at {
            8 | 13 | 18 | 23 => b == b'-',
            _ => matches!(b, b'0'..=b'9' | b'a'..=b'f'),
        })
}

/// Why a [`crate::Body::CopyGrant`] is malformed, or `None`: a store id, a
/// marketplace (empty when the store has none) and a product id.
#[must_use]
pub fn copy_grant_problem(
    store: &str,
    marketplace: &str,
    product_id: &str,
) -> Option<&'static str> {
    if !is_store_id(store) {
        return Some("invalid_store");
    }
    if !marketplace.is_empty() && !is_marketplace(marketplace) {
        return Some("invalid_marketplace");
    }
    if !is_product_id(product_id) {
        return Some("invalid_product_id");
    }
    None
}

/// The fields of a [`crate::Body::CopyGranted`], borrowed for
/// [`copy_granted_problem`].
#[derive(Debug, Clone, Copy)]
pub struct CopyGrantedFrame<'a> {
    /// The copy id.
    pub copy_id: &'a str,
    /// The sealed token text.
    pub sealed_token: &'a str,
    /// Seconds the token stays valid.
    pub expires_in_secs: u64,
    /// What the copy is.
    pub copy: &'a WindowCopy,
    /// The copy's files.
    pub files: &'a [CopyFile],
}

/// Why a [`crate::Body::CopyGranted`] is malformed, or `None`: a copy id
/// ([`is_copy_id`]), a non-empty sealed token of base64url text, a life of
/// 1 to [`MAX_COPY_TOKEN_SECS`] seconds, a well-formed copy, and 1 to
/// [`MAX_COPY_FILES`] well-formed files with distinct names, exactly one
/// of them the primary audio ([`CopyRole::Audio`]).
#[must_use]
pub fn copy_granted_problem(frame: &CopyGrantedFrame<'_>) -> Option<&'static str> {
    if !is_copy_id(frame.copy_id) {
        return Some("invalid_copy_id");
    }
    let token_ok = !frame.sealed_token.is_empty()
        && frame.sealed_token.len() <= 1024
        && frame
            .sealed_token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !token_ok {
        return Some("invalid_token");
    }
    if frame.expires_in_secs == 0 || frame.expires_in_secs > MAX_COPY_TOKEN_SECS {
        return Some("invalid_expiry");
    }
    if !frame.copy.is_well_formed() {
        return Some("invalid_copy");
    }
    if frame.files.is_empty() || frame.files.len() > MAX_COPY_FILES {
        return Some("invalid_files");
    }
    let mut names = std::collections::BTreeSet::new();
    let mut primaries = 0usize;
    for file in frame.files {
        if !file.is_well_formed() || !names.insert(file.name.as_str()) {
            return Some("invalid_files");
        }
        primaries += usize::from(file.role == CopyRole::Audio);
    }
    if primaries != 1 {
        return Some("invalid_files");
    }
    None
}

/// What a download's `Range` header asks for ([`parse_copy_range`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyRange {
    /// The whole file (answer 200): no usable range was named.
    Whole,
    /// `length` bytes from `offset` (answer 206); `length` is at least 1
    /// and the range lies within the file.
    Part {
        /// First byte.
        offset: u64,
        /// Number of bytes.
        length: u64,
    },
    /// A well-formed range that starts at or past the end of the file
    /// (answer 416).
    Unsatisfiable,
}

/// Reads the `Range` header of a download of a file of `size` bytes.
///
/// One byte range is honoured: `bytes=A-B` (clamped to the file's end),
/// `bytes=A-` and the suffix form `bytes=-N` (the last `N` bytes). Each
/// number is 1 to 19 decimal digits. Anything else (another unit, several
/// ranges, spaces, signs, `A` past `B`) is ignored and reads the whole
/// file, as HTTP allows a server to do.
#[must_use]
pub fn parse_copy_range(header: &str, size: u64) -> CopyRange {
    /// 1 to 19 ASCII digits as a number.
    fn number(text: &str) -> Option<u64> {
        (!text.is_empty() && text.len() <= 19 && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse().ok())
            .flatten()
    }
    let Some((first, last)) = header
        .strip_prefix("bytes=")
        .and_then(|spec| spec.split_once('-'))
    else {
        return CopyRange::Whole;
    };
    match (first.is_empty(), last.is_empty()) {
        (true, true) => CopyRange::Whole,
        (true, false) => match number(last) {
            None => CopyRange::Whole,
            Some(0) => CopyRange::Unsatisfiable,
            Some(_) if size == 0 => CopyRange::Unsatisfiable,
            Some(suffix) => {
                let length = suffix.min(size);
                CopyRange::Part {
                    offset: size - length,
                    length,
                }
            }
        },
        (false, open) => {
            let Some(offset) = number(first) else {
                return CopyRange::Whole;
            };
            let end = if open {
                None
            } else {
                match number(last) {
                    Some(end) if end >= offset => Some(end),
                    _ => return CopyRange::Whole,
                }
            };
            if offset >= size {
                return CopyRange::Unsatisfiable;
            }
            let last_byte = end.map_or(size - 1, |end| end.min(size - 1));
            CopyRange::Part {
                offset,
                length: last_byte - offset + 1,
            }
        }
    }
}

/// Longest head of a download's answer a reader takes, in bytes: the
/// status line and headers, before the blank line.
pub const MAX_COPY_ANSWER_HEAD_BYTES: usize = 16 * 1024;

/// Finds where the head of a download's answer ends in the bytes
/// received so far, however the library's writes were cut into reads.
///
/// The head ends before the first blank line (CRLF CRLF). A reader calls
/// this on everything it has received after each read: the head is the
/// first `n` bytes, [`parse_copy_answer`] reads it, and the body starts
/// four bytes after it. The answer depends only on the bytes, never on
/// where the reads fell: once it is `Some(n)` or an error it stays that
/// for every longer run of the same bytes.
///
/// # Returns
///
/// `Ok(Some(n))` when the head is the first `n` bytes, `Ok(None)` when
/// the blank line has not arrived yet and still may.
///
/// # Errors
///
/// Returns a short reason when the head would be longer than
/// [`MAX_COPY_ANSWER_HEAD_BYTES`].
pub fn copy_answer_head_end(received: &[u8]) -> Result<Option<usize>, &'static str> {
    match received.windows(4).position(|run| run == b"\r\n\r\n") {
        Some(at) if at <= MAX_COPY_ANSWER_HEAD_BYTES => Ok(Some(at)),
        Some(_) => Err("head_too_long"),
        // A blank line that has not started within the limit never ends
        // a head short enough.
        None if received.len() >= MAX_COPY_ANSWER_HEAD_BYTES + 4 => Err("head_too_long"),
        None => Ok(None),
    }
}

/// What the head of a download's answer says follows it
/// ([`parse_copy_answer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyAnswer {
    /// `length` bytes of the file follow, from byte `offset` to its end:
    /// `offset + length` is the file's size, and `offset` is 0 or the
    /// byte the reader asked to resume from.
    Body {
        /// First byte of the file the body holds.
        offset: u64,
        /// Number of bytes that follow.
        length: u64,
    },
    /// The library answered with this status and no file bytes: a refusal
    /// (401, 404), an unsatisfiable range (416) or a fault.
    Status(u16),
}

/// Reads the head of the answer to a download of a file of `size` bytes
/// that asked for the bytes from `asked_from` on (0 for the whole file).
///
/// `head` is the status line and the headers, each line ended by CRLF or
/// the end of the head, without the blank line that ends it. A reader
/// writes the body where this says, so the head must state exactly the
/// bytes that were asked for:
///
/// - `200` with `Content-Length` equal to `size`: the whole file, also
///   when a range was asked for (a server may ignore one);
/// - `206` with `Content-Range: bytes <asked_from>-<size - 1>/<size>` and
///   the matching `Content-Length`, only when `asked_from` is above 0 and
///   inside the file;
/// - any other status from 100 to 599: no file bytes. A `416` that
///   states a `Content-Range` must state the unsatisfied form,
///   `bytes */<size>`.
///
/// A header's name is an HTTP token (letters, digits and
/// ``!#$%&'*+-.^_`|~``).
///
/// # Errors
///
/// Returns a short reason when the head is longer than
/// [`MAX_COPY_ANSWER_HEAD_BYTES`], is not printable ASCII lines, has no
/// `HTTP/1.x` status line, states a transfer encoding, repeats or
/// misstates `Content-Length` or `Content-Range`, or answers `200` or
/// `206` with other bytes than the ones asked for.
pub fn parse_copy_answer(
    head: &[u8],
    asked_from: u64,
    size: u64,
) -> Result<CopyAnswer, &'static str> {
    /// 1 to 19 ASCII digits as a number.
    fn number(text: &str) -> Option<u64> {
        (!text.is_empty() && text.len() <= 19 && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse().ok())
            .flatten()
    }
    /// True for a byte of an HTTP token (RFC 9110 `tchar`).
    fn token_byte(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
    }
    if head.len() > MAX_COPY_ANSWER_HEAD_BYTES {
        return Err("head_too_long");
    }
    if !head
        .iter()
        .all(|byte| matches!(byte, b' '..=b'~' | b'\r' | b'\n' | b'\t'))
    {
        return Err("head_not_text");
    }
    let text = std::str::from_utf8(head).map_err(|_| "head_not_text")?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = Some(status_line)
        .filter(|line| !line.contains(['\r', '\n']))
        .ok_or("bad_status_line")?
        .strip_prefix("HTTP/1.")
        .and_then(|rest| rest.strip_prefix(['0', '1']))
        .and_then(|rest| rest.strip_prefix(' '))
        .and_then(|rest| {
            rest.get(..3)
                .map(|code| (code, rest.get(3..).unwrap_or("")))
        })
        .filter(|(_, reason)| reason.is_empty() || reason.starts_with(' '))
        .and_then(|(code, _)| number(code))
        .and_then(|code| u16::try_from(code).ok())
        .filter(|code| (100..=599).contains(code))
        .ok_or("bad_status_line")?;
    let mut length = None;
    let mut range = None;
    for line in lines {
        if line.contains(['\r', '\n']) {
            return Err("bad_header_line");
        }
        let (name, value) = line.split_once(':').ok_or("bad_header_line")?;
        if name.is_empty() || !name.bytes().all(token_byte) {
            return Err("bad_header_line");
        }
        let value = value.trim_matches([' ', '\t']);
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err("transfer_encoding");
        } else if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err("bad_content_length");
            }
            length = Some(number(value).ok_or("bad_content_length")?);
        } else if name.eq_ignore_ascii_case("content-range") {
            if range.is_some() {
                return Err("bad_content_range");
            }
            range = Some(value);
        }
    }
    match status {
        200 => {
            if range.is_some() {
                return Err("bad_content_range");
            }
            if length != Some(size) {
                return Err("bad_content_length");
            }
            Ok(CopyAnswer::Body {
                offset: 0,
                length: size,
            })
        }
        206 => {
            if asked_from == 0 || asked_from >= size {
                return Err("unasked_range");
            }
            let (first, last, total) = range
                .and_then(|value| value.strip_prefix("bytes "))
                .and_then(|spec| spec.split_once('/'))
                .and_then(|(span, total)| {
                    let (first, last) = span.split_once('-')?;
                    Some((number(first)?, number(last)?, number(total)?))
                })
                .ok_or("bad_content_range")?;
            if first != asked_from || total != size || last != size - 1 {
                return Err("bad_content_range");
            }
            let rest = size - asked_from;
            if length != Some(rest) {
                return Err("bad_content_length");
            }
            Ok(CopyAnswer::Body {
                offset: asked_from,
                length: rest,
            })
        }
        416 => {
            let stated = range.map(|value| {
                value
                    .strip_prefix("bytes */")
                    .and_then(number)
                    .ok_or("bad_content_range")
            });
            stated.transpose()?;
            Ok(CopyAnswer::Status(416))
        }
        other => Ok(CopyAnswer::Status(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed primary audio file.
    fn audio(name: &str) -> CopyFile {
        CopyFile {
            name: name.into(),
            role: CopyRole::Audio,
            size: 10,
            content_type: Some("audio/mp4".into()),
            probe: Some(UploadProbe {
                bitrate_kbps: Some(125),
                lossless: false,
                source_store: Some("audible".into()),
            }),
        }
    }

    /// A well-formed copy.
    fn copy() -> WindowCopy {
        WindowCopy {
            source_store: None,
            content_type: "audio/mp4".into(),
            bitrate_kbps: None,
            lossless: false,
            version: "v1".into(),
        }
    }

    /// A copy id.
    const ID: &str = "6f1c2b7e-0d4a-4e8b-9a51-2c3d4e5f6a7b";

    #[test]
    fn grants_are_validated() {
        assert_eq!(copy_grant_problem("audible", "us", "B0TEST"), None);
        assert_eq!(copy_grant_problem("libro", "", "9780000000001"), None);
        assert_eq!(
            copy_grant_problem("Audible", "us", "B0TEST"),
            Some("invalid_store")
        );
        assert_eq!(
            copy_grant_problem("audible", "US!", "B0TEST"),
            Some("invalid_marketplace")
        );
        assert_eq!(
            copy_grant_problem("audible", "us", " "),
            Some("invalid_product_id")
        );
    }

    #[test]
    fn copy_ids_are_lowercase_uuids() {
        assert!(is_copy_id(ID));
        assert!(!is_copy_id(&ID.to_uppercase()));
        assert!(!is_copy_id(&ID[1..]));
        assert!(!is_copy_id(&ID.replace('-', "0")));
        assert!(!is_copy_id("../../../../../../../../../etc/pass"));
    }

    #[test]
    fn answers_are_validated() {
        let copy = copy();
        let files = vec![
            audio("Title.m4b"),
            CopyFile {
                name: "cover.jpg".into(),
                role: CopyRole::Cover,
                size: 0,
                content_type: None,
                probe: None,
            },
        ];
        let ok = CopyGrantedFrame {
            copy_id: ID,
            sealed_token: "abc-_09",
            expires_in_secs: 3600,
            copy: &copy,
            files: &files,
        };
        assert_eq!(copy_granted_problem(&ok), None);
        let bad = |change: &dyn Fn(&mut CopyGrantedFrame<'_>)| {
            let mut frame = ok;
            change(&mut frame);
            copy_granted_problem(&frame)
        };
        assert_eq!(bad(&|f| f.copy_id = "nope"), Some("invalid_copy_id"));
        assert_eq!(bad(&|f| f.sealed_token = ""), Some("invalid_token"));
        assert_eq!(bad(&|f| f.sealed_token = "a b"), Some("invalid_token"));
        assert_eq!(bad(&|f| f.expires_in_secs = 0), Some("invalid_expiry"));
        assert_eq!(
            bad(&|f| f.expires_in_secs = MAX_COPY_TOKEN_SECS + 1),
            Some("invalid_expiry")
        );
        let bad_copy = WindowCopy {
            version: String::new(),
            ..copy.clone()
        };
        let mut frame = ok;
        frame.copy = &bad_copy;
        assert_eq!(copy_granted_problem(&frame), Some("invalid_copy"));

        let with = |files: &[CopyFile]| {
            let mut frame = ok;
            frame.files = files;
            copy_granted_problem(&frame)
        };
        assert_eq!(with(&[]), Some("invalid_files"));
        // No primary, two primaries, a repeated name.
        assert_eq!(with(&files[1..]), Some("invalid_files"));
        assert_eq!(
            with(&[audio("a.m4b"), audio("b.m4b")]),
            Some("invalid_files")
        );
        assert_eq!(
            with(&[audio("a.m4b"), files[1].clone(), files[1].clone()]),
            Some("invalid_files")
        );
        // A name that leaves the folder, a size past the cap, a bad type,
        // a bad probe.
        for change in [
            (|f: &mut CopyFile| f.name = "../a.m4b".into()) as fn(&mut CopyFile),
            |f| f.name = format!("{}.m4b", "a".repeat(MAX_COPY_NAME_BYTES)),
            |f| f.size = MAX_COPY_FILE_BYTES + 1,
            |f| f.content_type = Some("Audio/MP4".into()),
            |f| f.probe.as_mut().unwrap().bitrate_kbps = Some(0),
        ] {
            let mut file = audio("a.m4b");
            change(&mut file);
            assert_eq!(with(&[file]), Some("invalid_files"));
        }
        let many: Vec<CopyFile> = std::iter::once(audio("a.m4b"))
            .chain((0..MAX_COPY_FILES).map(|n| CopyFile {
                name: format!("c/{n}.mp3"),
                role: CopyRole::Chapter,
                size: 1,
                content_type: None,
                probe: None,
            }))
            .collect();
        assert_eq!(with(&many), Some("invalid_files"));
        assert_eq!(with(&many[..MAX_COPY_FILES]), None);
    }

    #[test]
    fn ranges_are_read() {
        use CopyRange::{Part, Unsatisfiable, Whole};
        let part = |offset, length| Part { offset, length };
        for (header, size, want) in [
            ("bytes=0-", 10, part(0, 10)),
            ("bytes=3-", 10, part(3, 7)),
            ("bytes=3-5", 10, part(3, 3)),
            ("bytes=3-3", 10, part(3, 1)),
            ("bytes=3-99", 10, part(3, 7)),
            ("bytes=9-", 10, part(9, 1)),
            ("bytes=-4", 10, part(6, 4)),
            ("bytes=-99", 10, part(0, 10)),
            ("bytes=10-", 10, Unsatisfiable),
            ("bytes=10-12", 10, Unsatisfiable),
            ("bytes=-0", 10, Unsatisfiable),
            ("bytes=0-", 0, Unsatisfiable),
            ("bytes=-5", 0, Unsatisfiable),
            ("", 10, Whole),
            ("bytes=", 10, Whole),
            ("bytes=-", 10, Whole),
            ("bytes=5-3", 10, Whole),
            ("bytes=0-1,3-4", 10, Whole),
            ("bytes= 0-1", 10, Whole),
            ("bytes=+0-1", 10, Whole),
            ("bytes=0x1-", 10, Whole),
            ("items=0-1", 10, Whole),
            ("Bytes=0-1", 10, Whole),
            ("bytes=99999999999999999999-", 10, Whole),
            ("bytes=0-99999999999999999999", 10, Whole),
        ] {
            assert_eq!(parse_copy_range(header, size), want, "{header:?} of {size}");
        }
        assert_eq!(
            parse_copy_range("bytes=0-", u64::MAX),
            part(0, u64::MAX),
            "a file of any size"
        );
    }

    #[test]
    fn frames_round_trip() {
        let grant = crate::Frame::new(
            "1",
            crate::Body::CopyGrant {
                store: "libro".into(),
                marketplace: String::new(),
                product_id: "9780000000001".into(),
            },
        );
        let text = grant.to_text();
        assert_eq!(
            text,
            r#"{"v":1,"id":"1","type":"copy.grant","store":"libro","product_id":"9780000000001"}"#
        );
        assert_eq!(crate::Frame::parse(&text).unwrap(), grant);
        let granted = crate::Frame::new(
            "1",
            crate::Body::CopyGranted {
                copy_id: ID.into(),
                sealed_token: "abc".into(),
                expires_in_secs: 3600,
                copy: copy(),
                files: vec![audio("Title.m4b")],
            },
        );
        assert_eq!(crate::Frame::parse(&granted.to_text()).unwrap(), granted);
    }

    #[test]
    fn files_are_written_without_absent_fields() {
        let file = CopyFile {
            name: "cover.jpg".into(),
            role: CopyRole::Cover,
            size: 3,
            content_type: None,
            probe: None,
        };
        assert_eq!(
            serde_json::to_string(&file).unwrap(),
            r#"{"name":"cover.jpg","role":"cover","size":3}"#
        );
        assert!(serde_json::from_str::<CopyFile>(
            r#"{"name":"a","role":"cover","size":3,"key":"x"}"#
        )
        .is_err());
        assert!(
            serde_json::from_str::<CopyFile>(r#"{"name":"a","role":"video","size":3}"#).is_err()
        );
    }

    #[test]
    fn a_download_answer_head_ends_at_the_first_blank_line_however_it_arrives() {
        let answer = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nab\r\n\r\n";
        let head = b"HTTP/1.1 200 OK\r\nContent-Length: 4".len();
        for received in 0..=answer.len() {
            let want = (received >= head + 4).then_some(head);
            assert_eq!(copy_answer_head_end(&answer[..received]), Ok(want));
        }
        // The longest head is taken; one byte more is not, with or
        // without its blank line.
        let mut longest = vec![b'a'; MAX_COPY_ANSWER_HEAD_BYTES];
        assert_eq!(copy_answer_head_end(&longest), Ok(None));
        longest.extend_from_slice(b"\r\n\r");
        assert_eq!(copy_answer_head_end(&longest), Ok(None));
        longest.push(b'\n');
        assert_eq!(
            copy_answer_head_end(&longest),
            Ok(Some(MAX_COPY_ANSWER_HEAD_BYTES))
        );
        let mut over = vec![b'a'; MAX_COPY_ANSWER_HEAD_BYTES + 1];
        over.extend_from_slice(b"\r\n\r");
        assert_eq!(copy_answer_head_end(&over), Err("head_too_long"));
        over.push(b'\n');
        assert_eq!(copy_answer_head_end(&over), Err("head_too_long"));
    }

    #[test]
    fn a_download_answer_states_exactly_the_bytes_asked_for() {
        use CopyAnswer::{Body, Status};
        let body = |offset, length| Ok(Body { offset, length });
        for (head, from, size, want) in [
            ("HTTP/1.1 200 OK\r\nContent-Length: 10", 0, 10, body(0, 10)),
            ("HTTP/1.1 200 OK\r\ncontent-length:10", 4, 10, body(0, 10)),
            ("HTTP/1.0 200\r\nContent-Length: 0", 0, 0, body(0, 0)),
            (
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 4-9/10\r\nContent-Length: 6",
                4,
                10,
                body(4, 6),
            ),
            (
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: 24",
                0,
                10,
                Ok(Status(401)),
            ),
            (
                "HTTP/1.1 416 Range Not Satisfiable",
                10,
                10,
                Ok(Status(416)),
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 9",
                0,
                10,
                Err("bad_content_length"),
            ),
            ("HTTP/1.1 200 OK", 0, 10, Err("bad_content_length")),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nContent-Length: 10",
                0,
                10,
                Err("bad_content_length"),
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nTransfer-Encoding: chunked",
                0,
                10,
                Err("transfer_encoding"),
            ),
            (
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 3-9/10\r\nContent-Length: 7",
                4,
                10,
                Err("bad_content_range"),
            ),
            (
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 4-8/10\r\nContent-Length: 5",
                4,
                10,
                Err("bad_content_range"),
            ),
            (
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 4-9/11\r\nContent-Length: 6",
                4,
                10,
                Err("bad_content_range"),
            ),
            (
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 4-9/10\r\nContent-Length: 5",
                4,
                10,
                Err("bad_content_length"),
            ),
            (
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/10\r\nContent-Length: 10",
                0,
                10,
                Err("unasked_range"),
            ),
            (
                "HTTP/2 200\r\nContent-Length: 10",
                0,
                10,
                Err("bad_status_line"),
            ),
            ("HTTP/1.1 2000 OK", 0, 10, Err("bad_status_line")),
            ("HTTP/1.1 099 Odd", 0, 10, Err("bad_status_line")),
            ("HTTP/1.1 200 OK\r\nno colon", 0, 10, Err("bad_header_line")),
            (
                "HTTP/1.1 200 OK\r\nBad(Name): x\r\nContent-Length: 10",
                0,
                10,
                Err("bad_header_line"),
            ),
            (
                "HTTP/1.1 200 OK\r\nX-Odd_Name.1: x\r\nContent-Length: 10",
                0,
                10,
                body(0, 10),
            ),
            (
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */10",
                10,
                10,
                Ok(Status(416)),
            ),
            (
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: invalid",
                10,
                10,
                Err("bad_content_range"),
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length : 10",
                0,
                10,
                Err("bad_header_line"),
            ),
            (
                "HTTP/1.1 200 OK\nContent-Length: 10",
                0,
                10,
                Err("bad_status_line"),
            ),
            ("HTTP/1.1 200 OK\r\nX: \u{e9}", 0, 10, Err("head_not_text")),
        ] {
            assert_eq!(
                parse_copy_answer(head.as_bytes(), from, size),
                want,
                "{head:?} from {from} of {size}"
            );
        }
        let long = format!(
            "HTTP/1.1 200 OK\r\nX: {}",
            "a".repeat(MAX_COPY_ANSWER_HEAD_BYTES)
        );
        assert_eq!(
            parse_copy_answer(long.as_bytes(), 0, 10),
            Err("head_too_long")
        );
    }
}
