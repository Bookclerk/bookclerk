//! Backblaze B2 native API replies, read into checked types.
//!
//! The `b2` destination talks to B2 through its native API (version 3):
//! `b2_authorize_account`, upload URLs, large-file parts, file-name and
//! file-version listings, deletes and server-side copies. Every reply comes
//! from outside Bookclerk, so this module is the boundary that reads them:
//! pure functions over bytes, with no I/O, fuzzed by the `b2_reply` target.
//!
//! The module also holds the request-side rules that must agree with what
//! it reads back: which file names B2 accepts ([`check_file_name`]) and how
//! a name is percent-encoded into an upload header or a download URL
//! ([`encode_file_name`]).
//!
//! Replies are capped at [`MAX_B2_REPLY_BYTES`]; ids, tokens, URLs, names
//! and file-info values each have their own cap, so a hostile or broken
//! service cannot make the guest hold more than a bounded amount.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Largest reply read, in bytes: a full listing of [`MAX_LIST_FILES`]
/// versions, each with its name and file info, fits well inside.
pub const MAX_B2_REPLY_BYTES: usize = 16 * 1024 * 1024;

/// Most files one listing reply may hold (B2's own `maxFileCount` limit).
pub const MAX_LIST_FILES: usize = 10_000;

/// Longest file name B2 accepts, in UTF-8 bytes.
pub const MAX_FILE_NAME_BYTES: usize = 1024;

/// Longest id (account, bucket, file) accepted, in bytes.
pub const MAX_ID_BYTES: usize = 256;

/// Longest authorization token accepted, in bytes.
pub const MAX_TOKEN_BYTES: usize = 4096;

/// Longest URL accepted, in bytes.
pub const MAX_URL_BYTES: usize = 2048;

/// Most file-info entries one file may carry in a reply.
pub const MAX_FILE_INFO_ENTRIES: usize = 64;

/// Longest file-info key or value accepted, in bytes.
pub const MAX_FILE_INFO_BYTES: usize = 8192;

/// Longest error code or message kept from an error reply, in bytes.
pub const MAX_ERROR_TEXT_BYTES: usize = 1024;

/// Most parts a large file may have (B2's limit).
pub const MAX_PARTS: u32 = 10_000;

/// Why a reply could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum B2ReplyError {
    /// The reply is over [`MAX_B2_REPLY_BYTES`].
    #[error("B2 reply is over {MAX_B2_REPLY_BYTES} bytes")]
    TooLarge,
    /// The reply is not the expected JSON shape, or a field breaks a rule.
    #[error("B2 reply is malformed: {0}")]
    Malformed(String),
}

/// Result alias for this module's parsers.
pub type ReplyResult<T> = std::result::Result<T, B2ReplyError>;

/// A [`B2ReplyError::Malformed`] with `what` as its detail.
fn malformed(what: impl Into<String>) -> B2ReplyError {
    B2ReplyError::Malformed(what.into())
}

/// Why a file name is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FileNameError {
    /// The name is empty.
    #[error("B2 file name is empty")]
    Empty,
    /// The name is over [`MAX_FILE_NAME_BYTES`] bytes.
    #[error("B2 file name is over {MAX_FILE_NAME_BYTES} bytes")]
    TooLong,
    /// The name holds a control character (below U+0020, or U+007F).
    #[error("B2 file name holds a control character")]
    Control,
    /// The name starts or ends with `/`, or holds `//`.
    #[error("B2 file name starts or ends with '/' or holds '//'")]
    Slash,
    /// The name holds a backslash, which B2 refuses.
    #[error("B2 file name holds a backslash")]
    Backslash,
}

/// Checks a file name against B2's rules: 1 to [`MAX_FILE_NAME_BYTES`]
/// bytes of UTF-8, no control characters or backslashes, and no leading,
/// trailing or doubled `/`.
///
/// # Arguments
///
/// * `name` - the full file name, prefix included.
///
/// # Errors
///
/// Returns the first rule the name breaks.
pub fn check_file_name(name: &str) -> std::result::Result<(), FileNameError> {
    if name.is_empty() {
        return Err(FileNameError::Empty);
    }
    if name.len() > MAX_FILE_NAME_BYTES {
        return Err(FileNameError::TooLong);
    }
    if name.chars().any(|c| c < ' ' || c == '\u{7f}') {
        return Err(FileNameError::Control);
    }
    if name.contains('\\') {
        return Err(FileNameError::Backslash);
    }
    if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
        return Err(FileNameError::Slash);
    }
    Ok(())
}

/// Whether `byte` stays as it is when percent-encoding a file name: the
/// RFC 3986 unreserved characters and `/`.
fn keeps(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/')
}

/// Percent-encodes a file name (or a file-info value) the way B2 reads the
/// `X-Bz-File-Name` header and the `/file/<bucket>/<name>` download path:
/// every byte outside the unreserved set and `/` becomes `%XX` (uppercase).
///
/// # Arguments
///
/// * `name` - the text to encode.
///
/// # Returns
///
/// The encoded text; [`decode_file_name`] gives `name` back.
#[must_use]
pub fn encode_file_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for &byte in name.as_bytes() {
        if keeps(byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
    }
    out
}

/// The uppercase hex digit for a nibble (`0..=15`).
fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => char::from(b'0' + nibble),
        _ => char::from(b'A' + (nibble - 10)),
    }
}

/// The value of one hex digit, or `None` for any other byte.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Decodes percent-encoding (`%XX`, and `+` as a space).
///
/// # Arguments
///
/// * `text` - encoded text, such as a file-info header value.
///
/// # Returns
///
/// The decoded text, or `None` when an escape is broken or the bytes are
/// not UTF-8.
#[must_use]
pub fn decode_file_name(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'%' => {
                let high = bytes.get(index + 1).copied().and_then(hex_value)?;
                let low = bytes.get(index + 2).copied().and_then(hex_value)?;
                out.push((high << 4) | low);
                index += 3;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            _ => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// An authorization token: for the account, or for one upload URL.
///
/// Its `Debug` output never shows the token.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthToken(String);

impl AuthToken {
    /// Checks a token: 1 to [`MAX_TOKEN_BYTES`] bytes of visible ASCII, so
    /// it is safe in an `Authorization` header.
    ///
    /// # Errors
    ///
    /// Returns [`B2ReplyError::Malformed`] for any other text.
    pub fn parse(token: &str) -> ReplyResult<Self> {
        if token.is_empty()
            || token.len() > MAX_TOKEN_BYTES
            || !token.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(malformed("authorization token is not a header-safe token"));
        }
        Ok(Self(token.to_string()))
    }

    /// The token, for the `Authorization` header.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthToken([redacted])")
    }
}

/// An `https` URL B2 handed out (API, download or upload URL), with no
/// user info, query or fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct B2Url(String);

impl B2Url {
    /// Checks a URL.
    ///
    /// # Errors
    ///
    /// Returns [`B2ReplyError::Malformed`] when the URL is not `https`, is
    /// over [`MAX_URL_BYTES`], has no host, or holds user info, a query, a
    /// fragment, whitespace or control characters.
    pub fn parse(url: &str) -> ReplyResult<Self> {
        let bad = || malformed("B2 URL is not a plain https URL");
        if url.len() > MAX_URL_BYTES || !url.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(bad());
        }
        let rest = url
            .strip_prefix("https://")
            .or_else(|| test_loopback(url))
            .ok_or_else(bad)?;
        let host = rest.split('/').next().unwrap_or_default();
        if host.is_empty()
            || host.contains('@')
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))
            || rest.contains(['?', '#'])
        {
            return Err(bad());
        }
        Ok(Self(url.trim_end_matches('/').to_string()))
    }

    /// The URL, without a trailing `/`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The URL's host (with its port, when it has one).
    #[must_use]
    pub fn host(&self) -> &str {
        self.0
            .strip_prefix("https://")
            .or_else(|| test_loopback(&self.0))
            .and_then(|rest| rest.split('/').next())
            .unwrap_or_default()
    }
}

/// In this crate's unit tests only, a plain-HTTP loopback URL (the fake B2
/// server has no certificate) reads as if it were `https`. Every other
/// build refuses it.
#[cfg(test)]
fn test_loopback(url: &str) -> Option<&str> {
    url.strip_prefix("http://")
        .filter(|rest| rest.starts_with("127.0.0.1:"))
}

/// Outside unit tests no plain-HTTP URL is accepted.
#[cfg(not(test))]
fn test_loopback(_url: &str) -> Option<&str> {
    None
}

/// Checks an id (account, bucket or file): 1 to [`MAX_ID_BYTES`] bytes of
/// visible ASCII.
fn check_id(what: &str, id: &str) -> ReplyResult<()> {
    if id.is_empty() || id.len() > MAX_ID_BYTES || !id.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(malformed(format!("{what} is not a B2 id")));
    }
    Ok(())
}

/// A B2 file id: one version of one file name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(String);

impl FileId {
    /// Checks a file id: 1 to [`MAX_ID_BYTES`] bytes of visible ASCII.
    ///
    /// # Errors
    ///
    /// Returns [`B2ReplyError::Malformed`] for any other text.
    pub fn parse(id: &str) -> ReplyResult<Self> {
        check_id("file id", id)?;
        Ok(Self(id.to_string()))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A bucket an application key may reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
    /// The bucket id, which API calls take.
    pub id: String,
    /// The bucket name, which download URLs take.
    pub name: String,
}

/// Checks a bucket name: 1 to 63 bytes of ASCII letters, digits and `-`
/// (B2's rule; names are unique across all of B2).
fn check_bucket_name(name: &str) -> ReplyResult<()> {
    if name.is_empty()
        || name.len() > 63
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(malformed("bucket name breaks B2's naming rule"));
    }
    Ok(())
}

/// What `b2_authorize_account` granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authorization {
    /// The account id.
    pub account_id: String,
    /// The token every other API call sends.
    pub token: AuthToken,
    /// Base URL of the API calls.
    pub api_url: B2Url,
    /// Base URL of downloads.
    pub download_url: B2Url,
    /// Part size B2 recommends for large files, in bytes.
    pub recommended_part_size: u64,
    /// Smallest part (other than the last) B2 accepts, in bytes.
    pub absolute_minimum_part_size: u64,
    /// The bucket the key is restricted to, when it is restricted to one.
    pub bucket: Option<Bucket>,
    /// The file-name prefix the key is restricted to, when it is.
    pub name_prefix: Option<String>,
    /// The capabilities the key holds (`listFiles`, `writeFiles`, ...).
    pub capabilities: Vec<String>,
}

impl Authorization {
    /// Whether the key holds `capability`.
    #[must_use]
    pub fn can(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|held| held == capability)
    }
}

/// A `b2_authorize_account` reply (API version 3).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireAuthorization {
    /// The account id.
    account_id: String,
    /// The account authorization token.
    authorization_token: String,
    /// Per-API information.
    api_info: WireApiInfo,
}

/// `apiInfo` in an authorization reply.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireApiInfo {
    /// The storage API's information.
    storage_api: WireStorageApi,
}

/// `apiInfo.storageApi` in an authorization reply.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireStorageApi {
    /// Base URL of API calls.
    api_url: String,
    /// Base URL of downloads.
    download_url: String,
    /// Recommended large-file part size.
    recommended_part_size: u64,
    /// Smallest large-file part size.
    absolute_minimum_part_size: u64,
    /// The key's restrictions.
    #[serde(default)]
    allowed: Option<WireAllowed>,
}

/// `allowed` in an authorization reply.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireAllowed {
    /// The buckets the key is restricted to (empty or absent: all).
    #[serde(default)]
    buckets: Option<Vec<WireAllowedBucket>>,
    /// The file-name prefix the key is restricted to.
    #[serde(default)]
    name_prefix: Option<String>,
    /// The capabilities the key holds.
    #[serde(default)]
    capabilities: Vec<String>,
}

/// One bucket in `allowed.buckets`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireAllowedBucket {
    /// The bucket id.
    id: String,
    /// The bucket name (null when the bucket was deleted).
    #[serde(default)]
    name: Option<String>,
}

/// Rejects a reply over [`MAX_B2_REPLY_BYTES`] and decodes the JSON.
fn decode<'a, T: Deserialize<'a>>(what: &str, bytes: &'a [u8]) -> ReplyResult<T> {
    if bytes.len() > MAX_B2_REPLY_BYTES {
        return Err(B2ReplyError::TooLarge);
    }
    // JSON is UTF-8 throughout, including fields the reader skips.
    let text = std::str::from_utf8(bytes).map_err(|_| malformed(format!("{what}: not UTF-8")))?;
    serde_json::from_str(text).map_err(|err| malformed(format!("{what}: {err}")))
}

/// Reads a `b2_authorize_account` reply.
///
/// # Arguments
///
/// * `bytes` - the reply body.
///
/// # Returns
///
/// What the key may do and where to send calls.
///
/// # Errors
///
/// Returns [`B2ReplyError`] for an oversized or malformed reply, a bad
/// URL, id or token, a part size of zero or a minimum above the
/// recommendation, or a restriction to more than one bucket.
pub fn parse_authorization(bytes: &[u8]) -> ReplyResult<Authorization> {
    let wire: WireAuthorization = decode("authorization reply", bytes)?;
    check_id("account id", &wire.account_id)?;
    let storage = wire.api_info.storage_api;
    if storage.absolute_minimum_part_size == 0
        || storage.recommended_part_size < storage.absolute_minimum_part_size
    {
        return Err(malformed("authorization reply has impossible part sizes"));
    }
    let (bucket, name_prefix, capabilities) = match storage.allowed {
        None => (None, None, Vec::new()),
        Some(allowed) => {
            let bucket = match allowed.buckets.unwrap_or_default().as_slice() {
                [] => None,
                [only] => {
                    check_id("bucket id", &only.id)?;
                    let name = only
                        .name
                        .clone()
                        .ok_or_else(|| malformed("the key's bucket no longer exists"))?;
                    check_bucket_name(&name)?;
                    Some(Bucket {
                        id: only.id.clone(),
                        name,
                    })
                }
                _ => return Err(malformed("the key is restricted to several buckets")),
            };
            let name_prefix = allowed.name_prefix.filter(|prefix| !prefix.is_empty());
            if let Some(prefix) = &name_prefix {
                if prefix.len() > MAX_FILE_NAME_BYTES || prefix.chars().any(char::is_control) {
                    return Err(malformed("the key's name prefix is not a file-name prefix"));
                }
            }
            if allowed.capabilities.len() > 64
                || allowed.capabilities.iter().any(|c| {
                    c.is_empty() || c.len() > 64 || !c.bytes().all(|b| b.is_ascii_alphanumeric())
                })
            {
                return Err(malformed("the key's capabilities are not capability names"));
            }
            (bucket, name_prefix, allowed.capabilities)
        }
    };
    Ok(Authorization {
        account_id: wire.account_id,
        token: AuthToken::parse(&wire.authorization_token)?,
        api_url: B2Url::parse(&storage.api_url)?,
        download_url: B2Url::parse(&storage.download_url)?,
        recommended_part_size: storage.recommended_part_size,
        absolute_minimum_part_size: storage.absolute_minimum_part_size,
        bucket,
        name_prefix,
        capabilities,
    })
}

/// A `b2_list_buckets` reply.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireBuckets {
    /// The buckets.
    buckets: Vec<WireBucket>,
}

/// One bucket in a `b2_list_buckets` reply.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireBucket {
    /// The bucket id.
    bucket_id: String,
    /// The bucket name.
    bucket_name: String,
}

/// Reads a `b2_list_buckets` reply.
///
/// # Arguments
///
/// * `bytes` - the reply body.
///
/// # Returns
///
/// The buckets it lists (at most 1,000, B2's per-account limit).
///
/// # Errors
///
/// Returns [`B2ReplyError`] for an oversized or malformed reply, or a bad
/// bucket id or name.
pub fn parse_buckets(bytes: &[u8]) -> ReplyResult<Vec<Bucket>> {
    let wire: WireBuckets = decode("bucket list", bytes)?;
    if wire.buckets.len() > 1000 {
        return Err(malformed("bucket list holds more than 1000 buckets"));
    }
    wire.buckets
        .into_iter()
        .map(|bucket| {
            check_id("bucket id", &bucket.bucket_id)?;
            check_bucket_name(&bucket.bucket_name)?;
            Ok(Bucket {
                id: bucket.bucket_id,
                name: bucket.bucket_name,
            })
        })
        .collect()
}

/// Where one upload (a small file, or one part of a large file) goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadTarget {
    /// The URL to `POST` the bytes to.
    pub url: B2Url,
    /// The token for that URL only.
    pub token: AuthToken,
}

/// A `b2_get_upload_url` or `b2_get_upload_part_url` reply.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireUploadTarget {
    /// The upload URL.
    upload_url: String,
    /// Its token.
    authorization_token: String,
}

/// Reads a `b2_get_upload_url` or `b2_get_upload_part_url` reply.
///
/// # Arguments
///
/// * `bytes` - the reply body.
///
/// # Returns
///
/// The upload URL and its token.
///
/// # Errors
///
/// Returns [`B2ReplyError`] for an oversized or malformed reply, or a bad
/// URL or token.
pub fn parse_upload_target(bytes: &[u8]) -> ReplyResult<UploadTarget> {
    let wire: WireUploadTarget = decode("upload URL reply", bytes)?;
    Ok(UploadTarget {
        url: B2Url::parse(&wire.upload_url)?,
        token: AuthToken::parse(&wire.authorization_token)?,
    })
}

/// What a file version is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    /// A stored file.
    Upload,
    /// A file made by a server-side copy.
    Copy,
    /// A hide marker: the name reads as deleted.
    Hide,
    /// A large file started and not yet finished.
    Start,
    /// A virtual folder (only in listings with a delimiter).
    Folder,
}

impl FileAction {
    /// Reads an `action` value.
    fn parse(action: &str) -> ReplyResult<Self> {
        match action {
            "upload" => Ok(Self::Upload),
            "copy" => Ok(Self::Copy),
            "hide" => Ok(Self::Hide),
            "start" => Ok(Self::Start),
            "folder" => Ok(Self::Folder),
            _ => Err(malformed("file action is not one B2 documents")),
        }
    }

    /// Whether the version holds the file's bytes (an upload or a copy).
    #[must_use]
    pub fn is_stored(self) -> bool {
        match self {
            Self::Upload | Self::Copy => true,
            Self::Hide | Self::Start | Self::Folder => false,
        }
    }
}

/// One version of one file, as B2 describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileVersion {
    /// The version's id.
    pub file_id: Option<FileId>,
    /// The full file name.
    pub file_name: String,
    /// The size in bytes (zero for hide markers and folders).
    pub content_length: u64,
    /// Lowercase hex SHA-1 of the bytes, when B2 knows it (small files, or
    /// large files that recorded `large_file_sha1`).
    pub content_sha1: Option<String>,
    /// The stored content type.
    pub content_type: Option<String>,
    /// When the version was uploaded, in UTC unix milliseconds.
    pub upload_timestamp_ms: u64,
    /// What the version is.
    pub action: FileAction,
    /// File info (`X-Bz-Info-*` values) stored with the version.
    pub file_info: BTreeMap<String, String>,
}

/// One file version in a reply.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireFile {
    /// The version id (null for folders).
    #[serde(default)]
    file_id: Option<String>,
    /// The file name.
    file_name: String,
    /// The size.
    #[serde(default)]
    content_length: u64,
    /// The SHA-1 (`none` for large files, `unverified:<hex>` for some).
    #[serde(default)]
    content_sha1: Option<String>,
    /// The content type.
    #[serde(default)]
    content_type: Option<String>,
    /// Upload time.
    #[serde(default)]
    upload_timestamp: u64,
    /// The version's action.
    action: String,
    /// File info.
    #[serde(default)]
    file_info: Option<BTreeMap<String, String>>,
}

/// Reads B2's `contentSha1` (or `large_file_sha1`) value: 40 hex digits,
/// optionally after `unverified:`; `none` and anything else read as
/// unknown.
fn sha1_of(value: Option<&str>) -> Option<String> {
    let value = value?;
    let hex = value.strip_prefix("unverified:").unwrap_or(value);
    (hex.len() == 40 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| hex.to_ascii_lowercase())
}

/// Checks one wire file version and turns it into a [`FileVersion`].
fn file_version(wire: WireFile) -> ReplyResult<FileVersion> {
    let action = FileAction::parse(&wire.action)?;
    let file_id = match (action, wire.file_id) {
        (FileAction::Folder, _) => None,
        (_, Some(id)) => Some(FileId::parse(&id)?),
        (_, None) => return Err(malformed("file version has no file id")),
    };
    match action {
        FileAction::Folder => {
            // A folder name ends with the delimiter; check the rest.
            let name = wire.file_name.strip_suffix('/').unwrap_or(&wire.file_name);
            check_file_name(name).map_err(|err| malformed(err.to_string()))?;
        }
        FileAction::Upload | FileAction::Copy | FileAction::Hide | FileAction::Start => {
            check_file_name(&wire.file_name).map_err(|err| malformed(err.to_string()))?;
        }
    }
    let info = wire.file_info.unwrap_or_default();
    if info.len() > MAX_FILE_INFO_ENTRIES
        || info.iter().any(|(key, value)| {
            key.len() > MAX_FILE_INFO_BYTES || value.len() > MAX_FILE_INFO_BYTES
        })
    {
        return Err(malformed("file info is over its limits"));
    }
    let content_type = match wire.content_type {
        Some(kind) if kind.len() > 256 || kind.chars().any(char::is_control) => {
            return Err(malformed("content type is not a media type"));
        }
        other => other.filter(|kind| !kind.is_empty()),
    };
    let content_sha1 = sha1_of(wire.content_sha1.as_deref())
        .or_else(|| sha1_of(info.get("large_file_sha1").map(String::as_str)));
    Ok(FileVersion {
        file_id,
        file_name: wire.file_name,
        content_length: wire.content_length,
        content_sha1,
        content_type,
        upload_timestamp_ms: wire.upload_timestamp,
        action,
        file_info: info,
    })
}

/// Reads a reply that describes one file version: an upload, a finished
/// large file, a copy, or `b2_start_large_file`.
///
/// # Arguments
///
/// * `bytes` - the reply body.
///
/// # Returns
///
/// The version.
///
/// # Errors
///
/// Returns [`B2ReplyError`] for an oversized or malformed reply, or a field
/// that breaks its rule.
pub fn parse_file_version(bytes: &[u8]) -> ReplyResult<FileVersion> {
    file_version(decode("file reply", bytes)?)
}

/// One page of `b2_list_file_names`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamesPage {
    /// The newest version of each name, in name order.
    pub files: Vec<FileVersion>,
    /// Where the next page starts, when there is one.
    pub next_file_name: Option<String>,
}

/// A `b2_list_file_names` reply.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireNamesPage {
    /// The files.
    files: Vec<WireFile>,
    /// The next page's first name.
    #[serde(default)]
    next_file_name: Option<String>,
}

/// Checks a listing's file count and versions.
fn listed(files: Vec<WireFile>) -> ReplyResult<Vec<FileVersion>> {
    if files.len() > MAX_LIST_FILES {
        return Err(malformed(format!(
            "listing holds more than {MAX_LIST_FILES} files"
        )));
    }
    files.into_iter().map(file_version).collect()
}

/// Checks a listing's continuation name: a file name, past every name the
/// page listed (so paging always moves forward).
fn next_name(next: Option<String>, files: &[FileVersion]) -> ReplyResult<Option<String>> {
    let Some(next) = next else {
        return Ok(None);
    };
    check_file_name(&next).map_err(|err| malformed(format!("next file name: {err}")))?;
    if files.iter().any(|file| file.file_name >= next) {
        return Err(malformed("next file name does not move past the page"));
    }
    Ok(Some(next))
}

/// Reads a `b2_list_file_names` reply.
///
/// # Arguments
///
/// * `bytes` - the reply body.
///
/// # Returns
///
/// The page.
///
/// # Errors
///
/// Returns [`B2ReplyError`] for an oversized or malformed reply, more than
/// [`MAX_LIST_FILES`] files, a bad version, or a continuation name that
/// does not move past the page.
pub fn parse_names_page(bytes: &[u8]) -> ReplyResult<NamesPage> {
    let wire: WireNamesPage = decode("file name listing", bytes)?;
    let files = listed(wire.files)?;
    let next_file_name = next_name(wire.next_file_name, &files)?;
    Ok(NamesPage {
        files,
        next_file_name,
    })
}

/// One page of `b2_list_file_versions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionsPage {
    /// Versions in name order, newest first within a name.
    pub files: Vec<FileVersion>,
    /// Where the next page starts (name and file id), when there is one.
    pub next: Option<(String, FileId)>,
}

/// A `b2_list_file_versions` reply.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireVersionsPage {
    /// The versions.
    files: Vec<WireFile>,
    /// The next page's first name.
    #[serde(default)]
    next_file_name: Option<String>,
    /// The next page's first file id.
    #[serde(default)]
    next_file_id: Option<String>,
}

/// Reads a `b2_list_file_versions` reply.
///
/// # Arguments
///
/// * `bytes` - the reply body.
///
/// # Returns
///
/// The page.
///
/// # Errors
///
/// Returns [`B2ReplyError`] for an oversized or malformed reply, more than
/// [`MAX_LIST_FILES`] files, a bad version, or a continuation with a name
/// but no id (or the reverse).
pub fn parse_versions_page(bytes: &[u8]) -> ReplyResult<VersionsPage> {
    let wire: WireVersionsPage = decode("file version listing", bytes)?;
    let files = listed(wire.files)?;
    let next = match (wire.next_file_name, wire.next_file_id) {
        (None, None) => None,
        (Some(name), Some(id)) => {
            check_file_name(&name).map_err(|err| malformed(format!("next file name: {err}")))?;
            if files.iter().any(|file| file.file_name > name) {
                return Err(malformed("next file name goes back before the page"));
            }
            Some((name, FileId::parse(&id)?))
        }
        _ => return Err(malformed("version listing continuation is half missing")),
    };
    Ok(VersionsPage { files, next })
}

/// A `b2_upload_part` reply.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WirePart {
    /// The part's number.
    part_number: u32,
    /// The part's SHA-1.
    content_sha1: String,
}

/// What `b2_upload_part` stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPart {
    /// The part's number (1 to [`MAX_PARTS`]).
    pub part_number: u32,
    /// Lowercase hex SHA-1 of the part.
    pub content_sha1: String,
}

/// Reads a `b2_upload_part` reply.
///
/// # Arguments
///
/// * `bytes` - the reply body.
///
/// # Returns
///
/// The part's number and SHA-1.
///
/// # Errors
///
/// Returns [`B2ReplyError`] for an oversized or malformed reply, a part
/// number outside 1 to [`MAX_PARTS`], or a SHA-1 that is not 40 hex
/// digits.
pub fn parse_part(bytes: &[u8]) -> ReplyResult<StoredPart> {
    let wire: WirePart = decode("part reply", bytes)?;
    if wire.part_number == 0 || wire.part_number > MAX_PARTS {
        return Err(malformed("part number is out of range"));
    }
    let content_sha1 =
        sha1_of(Some(&wire.content_sha1)).ok_or_else(|| malformed("part SHA-1 is not hex"))?;
    Ok(StoredPart {
        part_number: wire.part_number,
        content_sha1,
    })
}

/// What a download (`GET` or `HEAD` on `/file/<bucket>/<name>`) says
/// about the file in its response headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadHead {
    /// The version served. `content_length` is the whole file's size.
    pub version: FileVersion,
    /// Bytes in this response's body: the range length for a ranged read,
    /// the whole size otherwise.
    pub body_length: u64,
}

/// Most response headers read from one download reply.
pub const MAX_DOWNLOAD_HEADERS: usize = 128;

/// Reads a whole-file size out of a `Content-Range: bytes a-b/size` value,
/// with the range's length.
fn content_range(value: &str) -> Option<(u64, u64)> {
    let rest = value.strip_prefix("bytes ")?;
    let (span, size) = rest.split_once('/')?;
    let (start, end) = span.split_once('-')?;
    let start: u64 = start.parse().ok()?;
    let end: u64 = end.parse().ok()?;
    let size: u64 = size.parse().ok()?;
    if start > end || end >= size {
        return None;
    }
    Some((size, end - start + 1))
}

/// Reads the headers of a download reply.
///
/// # Arguments
///
/// * `headers` - each header's name (any case) and raw value, as received.
///
/// # Returns
///
/// The version served and the body's length.
///
/// # Errors
///
/// Returns [`B2ReplyError`] when there are more than
/// [`MAX_DOWNLOAD_HEADERS`] headers, a required header (`Content-Length`,
/// `X-Bz-File-Id`, `X-Bz-File-Name`) is missing or repeated, a value is not
/// text, a number does not parse, `Content-Range` disagrees with
/// `Content-Length`, or file info is over its limits.
pub fn parse_download_headers<'a, I>(headers: I) -> ReplyResult<DownloadHead>
where
    I: IntoIterator<Item = (&'a str, &'a [u8])>,
{
    let mut length = None;
    let mut range = None;
    let mut file_id = None;
    let mut file_name = None;
    let mut sha1 = None;
    let mut content_type = None;
    let mut uploaded = None;
    let mut info = BTreeMap::new();
    for (count, (name, value)) in headers.into_iter().enumerate() {
        if count >= MAX_DOWNLOAD_HEADERS {
            return Err(malformed("download reply has too many headers"));
        }
        let name = name.to_ascii_lowercase();
        let tracked = name == "content-length"
            || name == "content-range"
            || name == "content-type"
            || name.starts_with("x-bz-");
        if !tracked {
            continue;
        }
        let value = std::str::from_utf8(value)
            .map_err(|_| malformed(format!("header {name} is not text")))?;
        let once = |slot: &mut Option<String>| -> ReplyResult<()> {
            if slot.replace(value.to_string()).is_some() {
                return Err(malformed(format!("header {name} is repeated")));
            }
            Ok(())
        };
        match name.as_str() {
            "content-length" => once(&mut length)?,
            "content-range" => once(&mut range)?,
            "content-type" => once(&mut content_type)?,
            "x-bz-file-id" => once(&mut file_id)?,
            "x-bz-file-name" => once(&mut file_name)?,
            "x-bz-content-sha1" => once(&mut sha1)?,
            "x-bz-upload-timestamp" => once(&mut uploaded)?,
            other => {
                if let Some(key) = other.strip_prefix("x-bz-info-") {
                    let decoded = decode_file_name(value)
                        .ok_or_else(|| malformed("file info value is not percent-encoded text"))?;
                    if info.insert(key.to_string(), decoded).is_some() {
                        return Err(malformed("file info key is repeated"));
                    }
                }
            }
        }
    }
    let number = |what: &str, value: &str| -> ReplyResult<u64> {
        value
            .trim()
            .parse()
            .map_err(|_| malformed(format!("{what} is not a number")))
    };
    let body_length = number(
        "Content-Length",
        length
            .as_deref()
            .ok_or_else(|| malformed("download reply has no Content-Length"))?,
    )?;
    let total = match range.as_deref() {
        None => body_length,
        Some(value) => {
            let (total, span) = content_range(value.trim())
                .ok_or_else(|| malformed("Content-Range is malformed"))?;
            if span != body_length {
                return Err(malformed("Content-Range disagrees with Content-Length"));
            }
            total
        }
    };
    let file_name = decode_file_name(
        file_name
            .as_deref()
            .ok_or_else(|| malformed("download reply has no X-Bz-File-Name"))?,
    )
    .ok_or_else(|| malformed("X-Bz-File-Name is not percent-encoded text"))?;
    let upload_timestamp_ms = match uploaded.as_deref() {
        Some(value) => number("X-Bz-Upload-Timestamp", value)?,
        None => 0,
    };
    let wire = WireFile {
        file_id: Some(file_id.ok_or_else(|| malformed("download reply has no X-Bz-File-Id"))?),
        file_name,
        content_length: total,
        content_sha1: sha1,
        content_type,
        upload_timestamp: upload_timestamp_ms,
        action: "upload".into(),
        file_info: Some(info),
    };
    Ok(DownloadHead {
        version: file_version(wire)?,
        body_length,
    })
}

/// An error reply from the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    /// The HTTP status.
    pub status: u16,
    /// B2's error code (`expired_auth_token`, `not_found`, ...), or
    /// `unknown` when the body named none.
    pub code: String,
    /// B2's message, when there was one.
    pub message: String,
}

impl ApiError {
    /// Whether the account token expired, so a fresh authorization should
    /// be tried once.
    #[must_use]
    pub fn expired_token(&self) -> bool {
        self.status == 401 && matches!(self.code.as_str(), "expired_auth_token" | "bad_auth_token")
    }

    /// Whether B2 asked the caller to try again later (busy, too many
    /// requests, or an internal error).
    #[must_use]
    pub fn retryable(&self) -> bool {
        matches!(self.status, 408 | 429 | 500 | 503)
    }

    /// Whether the object (or file id) does not exist.
    #[must_use]
    pub fn not_found(&self) -> bool {
        self.status == 404 || self.code == "not_found" || self.code == "file_not_present"
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.is_empty() {
            write!(f, "B2 answered {} ({})", self.status, self.code)
        } else {
            write!(
                f,
                "B2 answered {} ({}): {}",
                self.status, self.code, self.message
            )
        }
    }
}

/// An error reply body.
#[derive(Deserialize)]
struct WireError {
    /// The error code.
    #[serde(default)]
    code: Option<String>,
    /// The message.
    #[serde(default)]
    message: Option<String>,
}

/// Cuts `text` to at most [`MAX_ERROR_TEXT_BYTES`] bytes on a character
/// boundary and drops control characters.
fn bounded_text(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > MAX_ERROR_TEXT_BYTES {
            break;
        }
        out.push(c);
    }
    out
}

/// Reads an error reply. Never fails: a body that is not B2's error JSON
/// reads as code `unknown` with no message.
///
/// # Arguments
///
/// * `status` - the HTTP status.
/// * `bytes` - the reply body.
///
/// # Returns
///
/// The error, with code and message cut to [`MAX_ERROR_TEXT_BYTES`].
#[must_use]
pub fn parse_api_error(status: u16, bytes: &[u8]) -> ApiError {
    let wire = (bytes.len() <= MAX_B2_REPLY_BYTES)
        .then(|| serde_json::from_slice::<WireError>(bytes).ok())
        .flatten();
    let (code, message) = match wire {
        Some(wire) => (wire.code, wire.message),
        None => (None, None),
    };
    let code = code
        .map(|code| bounded_text(&code))
        .filter(|code| !code.is_empty())
        .unwrap_or_else(|| "unknown".into());
    ApiError {
        status,
        code,
        message: message.map(|m| bounded_text(&m)).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full authorization reply restricted to `bucket`.
    fn authorization_json(bucket: &str) -> String {
        format!(
            r#"{{"accountId":"acct1","authorizationToken":"4_tok/en=","apiInfo":{{"storageApi":{{
              "apiUrl":"https://api005.backblazeb2.com","downloadUrl":"https://f005.backblazeb2.com/",
              "recommendedPartSize":100000000,"absoluteMinimumPartSize":5000000,
              "allowed":{{"buckets":[{bucket}],"namePrefix":"books/","capabilities":["listFiles","writeFiles"]}}}}}}}}"#
        )
    }

    #[test]
    fn file_names_follow_b2_rules() {
        assert_eq!(check_file_name("a/b c.m4b"), Ok(()));
        assert_eq!(check_file_name(""), Err(FileNameError::Empty));
        assert_eq!(
            check_file_name(&"x".repeat(1025)),
            Err(FileNameError::TooLong)
        );
        assert_eq!(check_file_name("a\nb"), Err(FileNameError::Control));
        assert_eq!(check_file_name("a\u{7f}"), Err(FileNameError::Control));
        assert_eq!(check_file_name("a\\b"), Err(FileNameError::Backslash));
        for bad in ["/a", "a/", "a//b"] {
            assert_eq!(check_file_name(bad), Err(FileNameError::Slash), "{bad}");
        }
    }

    #[test]
    fn names_round_trip_through_percent_encoding() {
        let name = "Author/Title: Part 1 (ünïcode) 100%+.m4b";
        let encoded = encode_file_name(name);
        assert_eq!(
            encoded,
            "Author/Title%3A%20Part%201%20%28%C3%BCn%C3%AFcode%29%20100%25%2B.m4b"
        );
        assert_eq!(decode_file_name(&encoded).as_deref(), Some(name));
        assert_eq!(decode_file_name("a+b").as_deref(), Some("a b"));
        assert_eq!(decode_file_name("%4"), None);
        assert_eq!(decode_file_name("%zz"), None);
        assert_eq!(decode_file_name("%FF"), None);
    }

    #[test]
    fn tokens_hide_in_debug_output() {
        let token = AuthToken::parse("secret-token").expect("token");
        assert_eq!(format!("{token:?}"), "AuthToken([redacted])");
        assert_eq!(token.expose(), "secret-token");
        assert!(AuthToken::parse("has space").is_err());
        assert!(AuthToken::parse("").is_err());
    }

    #[test]
    fn urls_must_be_plain_https() {
        let url = B2Url::parse("https://f005.backblazeb2.com/").expect("url");
        assert_eq!(url.as_str(), "https://f005.backblazeb2.com");
        assert_eq!(url.host(), "f005.backblazeb2.com");
        for bad in [
            "http://f005.backblazeb2.com",
            "https://user@host",
            "https://host/?q",
            "https://host/#f",
            "https:///path",
            "https://ho st",
        ] {
            assert!(B2Url::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn reads_a_bucket_restricted_authorization() {
        let auth =
            parse_authorization(authorization_json(r#"{"id":"b1","name":"my-books"}"#).as_bytes())
                .expect("authorization");
        assert_eq!(auth.account_id, "acct1");
        assert_eq!(auth.api_url.as_str(), "https://api005.backblazeb2.com");
        assert_eq!(auth.download_url.host(), "f005.backblazeb2.com");
        assert_eq!(auth.recommended_part_size, 100_000_000);
        assert_eq!(
            auth.bucket,
            Some(Bucket {
                id: "b1".into(),
                name: "my-books".into()
            })
        );
        assert_eq!(auth.name_prefix.as_deref(), Some("books/"));
        assert!(auth.can("writeFiles"));
        assert!(!auth.can("deleteFiles"));
    }

    #[test]
    fn refuses_impossible_authorizations() {
        let deleted = authorization_json(r#"{"id":"b1","name":null}"#);
        assert!(parse_authorization(deleted.as_bytes()).is_err());
        let several = authorization_json(r#"{"id":"b1","name":"a"},{"id":"b2","name":"b"}"#);
        assert!(parse_authorization(several.as_bytes()).is_err());
        let sizes = authorization_json(r#"{"id":"b1","name":"a"}"#).replace("100000000", "1");
        assert!(parse_authorization(sizes.as_bytes()).is_err());
        assert!(parse_authorization(b"{}").is_err());
        let huge = vec![b' '; MAX_B2_REPLY_BYTES + 1];
        assert_eq!(parse_authorization(&huge), Err(B2ReplyError::TooLarge));
    }

    #[test]
    fn reads_buckets_and_upload_targets() {
        let buckets = parse_buckets(
            br#"{"buckets":[{"bucketId":"b1","bucketName":"one","bucketType":"allPrivate"}]}"#,
        )
        .expect("buckets");
        assert_eq!(buckets.len(), 1);
        assert!(
            parse_buckets(br#"{"buckets":[{"bucketId":"b1","bucketName":"no_under"}]}"#).is_err()
        );
        let target = parse_upload_target(
            br#"{"bucketId":"b1","uploadUrl":"https://pod-000.backblaze.com/b2api/v3/b2_upload_file/b1/c0","authorizationToken":"t"}"#,
        )
        .expect("target");
        assert_eq!(target.url.host(), "pod-000.backblaze.com");
    }

    #[test]
    fn reads_file_versions() {
        let file = parse_file_version(
            br#"{"fileId":"4_z1","fileName":"a/b.m4b","contentLength":12,
                 "contentSha1":"unverified:ABCDEF0123456789ABCDEF0123456789ABCDEF01",
                 "contentType":"audio/mp4","uploadTimestamp":1700000000000,"action":"upload",
                 "fileInfo":{"src_last_modified_millis":"1"}}"#,
        )
        .expect("file");
        assert_eq!(file.file_id.as_ref().map(FileId::as_str), Some("4_z1"));
        assert_eq!(
            file.content_sha1.as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef01")
        );
        assert!(file.action.is_stored());
        let large = parse_file_version(
            br#"{"fileId":"4_z2","fileName":"big","contentSha1":"none","action":"upload",
                 "fileInfo":{"large_file_sha1":"0000000000000000000000000000000000000000"}}"#,
        )
        .expect("large file");
        assert_eq!(large.content_sha1.as_deref(), Some(&*"0".repeat(40)));
        assert!(parse_file_version(br#"{"fileName":"a","action":"upload"}"#).is_err());
        assert!(parse_file_version(br#"{"fileId":"x","fileName":"a","action":"zap"}"#).is_err());
        let folder = parse_file_version(br#"{"fileId":null,"fileName":"dir/","action":"folder"}"#)
            .expect("folder");
        assert!(folder.file_id.is_none());
        assert!(!folder.action.is_stored());
    }

    #[test]
    fn listings_must_move_forward() {
        let page = parse_names_page(
            br#"{"files":[{"fileId":"1","fileName":"a","action":"upload"}],"nextFileName":"b"}"#,
        )
        .expect("page");
        assert_eq!(page.next_file_name.as_deref(), Some("b"));
        assert!(parse_names_page(
            br#"{"files":[{"fileId":"1","fileName":"b","action":"upload"}],"nextFileName":"a"}"#,
        )
        .is_err());
        let versions = parse_versions_page(
            br#"{"files":[{"fileId":"2","fileName":"a","action":"upload"},{"fileId":"1","fileName":"a","action":"hide"}],
                 "nextFileName":"a","nextFileId":"0"}"#,
        )
        .expect("versions");
        assert_eq!(versions.files.len(), 2);
        assert_eq!(
            versions.next.as_ref().map(|(name, _)| name.as_str()),
            Some("a")
        );
        assert!(parse_versions_page(br#"{"files":[],"nextFileName":"a"}"#).is_err());
    }

    #[test]
    fn reads_parts() {
        let part = parse_part(
            br#"{"partNumber":3,"contentSha1":"ABCDEF0123456789ABCDEF0123456789ABCDEF01"}"#,
        )
        .expect("part");
        assert_eq!(part.part_number, 3);
        assert!(parse_part(br#"{"partNumber":0,"contentSha1":"x"}"#).is_err());
        assert!(parse_part(
            br#"{"partNumber":10001,"contentSha1":"0000000000000000000000000000000000000000"}"#
        )
        .is_err());
    }

    #[test]
    fn error_replies_always_read() {
        let expired = parse_api_error(
            401,
            br#"{"status":401,"code":"expired_auth_token","message":"expired"}"#,
        );
        assert!(expired.expired_token());
        assert_eq!(
            expired.to_string(),
            "B2 answered 401 (expired_auth_token): expired"
        );
        let junk = parse_api_error(503, b"<html>");
        assert_eq!(junk.code, "unknown");
        assert!(junk.retryable());
        let long = format!(r#"{{"code":"x","message":"{}"}}"#, "é".repeat(2000));
        assert!(parse_api_error(400, long.as_bytes()).message.len() <= MAX_ERROR_TEXT_BYTES);
        assert!(parse_api_error(404, b"{}").not_found());
    }
}
