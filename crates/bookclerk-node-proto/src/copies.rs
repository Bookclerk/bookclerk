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
}
