//! The host's key syntax, and how a storage backend turns a host key into
//! the path or object key it stores.
//!
//! The host names what it stores with one syntax, whatever the destination:
//! segments joined by `/`, filled from the destination's naming template.
//! Each storage plugin translates a host key into the key it actually uses
//! (a file-system path for `local`, an object key for an object store) and
//! returns it; the host records the translated key. The host never decides a
//! destination's character set or length rules itself (storage destinations
//! ADR).
//!
//! The syntax ([`HostKey`]):
//!
//! - one or more segments joined by `/`, the last naming the file;
//! - no segment is empty, `.` or `..`;
//! - no segment holds `/`, `\` or a control character;
//! - at most [`MAX_HOST_KEY_BYTES`] UTF-8 bytes in all.
//!
//! The translators here are the ones the first-party plugins use:
//! [`translate_for_filesystem`] and [`translate_for_object_store`].

use crate::error::{Result, StorageError};

/// Largest host key, in UTF-8 bytes. Matches the S3 object-key limit, so a
/// host key that fits fits an object store before its prefix.
pub const MAX_HOST_KEY_BYTES: usize = 1024;

/// Largest path component most file systems accept, in UTF-8 bytes on Unix
/// and UTF-16 units on Windows.
pub const MAX_FILESYSTEM_COMPONENT: usize = 255;

/// Largest object key S3-compatible stores accept, in UTF-8 bytes,
/// including the destination's prefix.
pub const MAX_OBJECT_KEY_BYTES: usize = 1024;

/// Longest extension a translator keeps whole when it shortens a file name,
/// in bytes (`.m4b`, `.metadata.json`).
const MAX_KEPT_EXTENSION_BYTES: usize = 16;

/// A key in the host's syntax: `/`-joined segments, validated on
/// construction.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostKey(String);

impl HostKey {
    /// Parses `key` as a host key.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::InvalidKey`] when `key` breaks the syntax in
    /// the module docs.
    pub fn parse(key: &str) -> Result<Self> {
        if key.is_empty() {
            return Err(StorageError::InvalidKey("empty host key".into()));
        }
        if key.len() > MAX_HOST_KEY_BYTES {
            return Err(StorageError::InvalidKey(format!(
                "host key is {} bytes, more than {MAX_HOST_KEY_BYTES}",
                key.len()
            )));
        }
        for segment in key.split('/') {
            check_segment(segment)?;
        }
        Ok(Self(key.to_string()))
    }

    /// The key as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The key's segments in order; the last names the file.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }
}

impl std::fmt::Display for HostKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Checks one host-key segment.
fn check_segment(segment: &str) -> Result<()> {
    if segment.is_empty() || segment == "." || segment == ".." {
        return Err(StorageError::InvalidKey(format!(
            "host key segment {segment:?} is empty or a dot segment"
        )));
    }
    if segment.chars().any(|c| c == '\\' || c.is_control()) {
        return Err(StorageError::InvalidKey(format!(
            "host key segment {segment:?} holds a backslash or a control character"
        )));
    }
    Ok(())
}

/// Characters Windows refuses in a file name, with what replaces each.
const WINDOWS_RESERVED: &[(char, char)] = &[
    (':', '_'),
    ('*', '_'),
    ('?', '_'),
    ('"', '\''),
    ('<', '('),
    ('>', ')'),
    ('|', '_'),
];

/// Device names Windows reserves, with or without an extension.
const WINDOWS_DEVICE_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Characters the S3 object-key guidelines say to avoid, with what replaces
/// each. Characters that only need URL encoding (`:`, `+`, spaces) stay.
const OBJECT_STORE_AVOIDED: &[(char, char)] = &[
    ('{', '_'),
    ('}', '_'),
    ('^', '_'),
    ('%', '_'),
    ('`', '_'),
    ('"', '\''),
    ('<', '('),
    ('>', ')'),
    ('[', '('),
    (']', ')'),
    ('#', '_'),
    ('|', '_'),
    ('~', '_'),
];

/// Which file-system naming rules apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemRules {
    /// Unix-like: only the component length matters.
    Unix,
    /// Windows: reserved characters and device names, no trailing dot or
    /// space, and the component length in UTF-16 units.
    Windows,
}

impl FilesystemRules {
    /// The rules for the platform this process runs on.
    #[must_use]
    pub fn native() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// Translates `key` into a relative path the file system under `rules`
/// accepts, segment by segment.
///
/// Each segment is made legal (on Windows: reserved characters replaced, a
/// device name prefixed with `_`, trailing dots and spaces dropped) and cut
/// to [`MAX_FILESYSTEM_COMPONENT`], keeping a short extension. The same
/// segment always translates the same way, so the files of one folder stay
/// in one folder.
#[must_use]
pub fn translate_for_filesystem(key: &HostKey, rules: FilesystemRules) -> String {
    key.segments()
        .map(|segment| {
            let legal = match rules {
                FilesystemRules::Unix => segment.to_string(),
                FilesystemRules::Windows => windows_segment(segment),
            };
            let measure = match rules {
                FilesystemRules::Unix => Measure::Utf8Bytes,
                FilesystemRules::Windows => Measure::Utf16Units,
            };
            shorten_keeping_extension(&legal, MAX_FILESYSTEM_COMPONENT, measure)
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// One segment made legal for Windows.
fn windows_segment(segment: &str) -> String {
    let replaced: String = segment
        .chars()
        .map(|c| {
            WINDOWS_RESERVED
                .iter()
                .find(|(reserved, _)| *reserved == c)
                .map_or(c, |(_, with)| *with)
        })
        .collect();
    let trimmed = replaced.trim_end_matches(['.', ' ']);
    let trimmed = if trimmed.is_empty() { "_" } else { trimmed };
    let stem = trimmed.split('.').next().unwrap_or(trimmed);
    if WINDOWS_DEVICE_NAMES
        .iter()
        .any(|name| name.eq_ignore_ascii_case(stem.trim_end()))
    {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// Translates `key` into an object key an S3-compatible store accepts
/// under a prefix of `prefix_bytes` bytes.
///
/// Characters the S3 guidelines say to avoid are replaced, and when the
/// prefixed key would pass [`MAX_OBJECT_KEY_BYTES`] the file name's stem is
/// shortened first, then the longest folder segments, so the key keeps its
/// folders and its extension.
#[must_use]
pub fn translate_for_object_store(key: &HostKey, prefix_bytes: usize) -> String {
    let mut segments: Vec<String> = key
        .segments()
        .map(|segment| {
            segment
                .chars()
                .map(|c| {
                    OBJECT_STORE_AVOIDED
                        .iter()
                        .find(|(avoided, _)| *avoided == c)
                        .map_or(c, |(_, with)| *with)
                })
                .collect()
        })
        .collect();
    let budget = MAX_OBJECT_KEY_BYTES.saturating_sub(prefix_bytes);
    fit_total(&mut segments, budget);
    segments.join("/")
}

/// Shortens `segments` until they join to at most `budget` bytes: the last
/// segment's stem first, down to one character before its extension, then
/// the longest folder segment, one at a time, and the file name itself
/// only when the folders are down to one character each.
fn fit_total(segments: &mut [String], budget: usize) {
    let total = |segments: &[String]| {
        segments.iter().map(String::len).sum::<usize>() + segments.len().saturating_sub(1)
    };
    let over = total(segments).saturating_sub(budget);
    if over == 0 {
        return;
    }
    if let Some(last) = segments.last_mut() {
        let floor = kept_extension(last).map_or(1, |ext| ext.len() + 1);
        let target = last.len().saturating_sub(over).max(floor);
        *last = shorten_keeping_extension(last, target, Measure::Utf8Bytes);
    }
    while total(segments) > budget {
        let folders = segments.len().saturating_sub(1);
        let Some((index, longest)) = segments[..folders]
            .iter()
            .enumerate()
            .max_by_key(|(_, segment)| segment.len())
            .map(|(index, segment)| (index, segment.len()))
        else {
            break;
        };
        if longest <= 1 {
            break;
        }
        let over = total(segments) - budget;
        let target = longest.saturating_sub(over).max(1);
        segments[index] = cut(&segments[index], target, Measure::Utf8Bytes);
    }
    let over = total(segments).saturating_sub(budget);
    if over > 0 {
        if let Some(last) = segments.last_mut() {
            let target = last.len().saturating_sub(over).max(1);
            *last = shorten_keeping_extension(last, target, Measure::Utf8Bytes);
        }
    }
}

/// The extension [`shorten_keeping_extension`] keeps on `segment`: from
/// its last `.` (not a leading one), at most [`MAX_KEPT_EXTENSION_BYTES`].
fn kept_extension(segment: &str) -> Option<&str> {
    segment
        .rfind('.')
        .filter(|&dot| dot > 0)
        .map(|dot| &segment[dot..])
        .filter(|ext| ext.len() <= MAX_KEPT_EXTENSION_BYTES)
}

/// How a translator counts a segment's length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Measure {
    /// UTF-8 bytes.
    Utf8Bytes,
    /// UTF-16 code units (Windows file names).
    Utf16Units,
}

/// Length of `c` under `measure`.
fn char_len(c: char, measure: Measure) -> usize {
    match measure {
        Measure::Utf8Bytes => c.len_utf8(),
        Measure::Utf16Units => c.len_utf16(),
    }
}

/// Length of `text` under `measure`.
fn text_len(text: &str, measure: Measure) -> usize {
    text.chars().map(|c| char_len(c, measure)).sum()
}

/// The longest prefix of `text` at most `limit` long, on a character
/// boundary, with trailing spaces and dots dropped. Never empty when `text`
/// is not.
fn cut(text: &str, limit: usize, measure: Measure) -> String {
    let mut used = 0;
    let mut out = String::new();
    for c in text.chars() {
        used += char_len(c, measure);
        if used > limit {
            break;
        }
        out.push(c);
    }
    let trimmed = out.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        text.chars().next().map(String::from).unwrap_or_default()
    } else {
        trimmed.to_string()
    }
}

/// `segment` at most `limit` long, shortening the stem and keeping an
/// extension of up to [`MAX_KEPT_EXTENSION_BYTES`].
fn shorten_keeping_extension(segment: &str, limit: usize, measure: Measure) -> String {
    if text_len(segment, measure) <= limit {
        return segment.to_string();
    }
    match kept_extension(segment) {
        Some(ext) if text_len(ext, measure) < limit => {
            let stem = &segment[..segment.len() - ext.len()];
            let room = limit - text_len(ext, measure);
            format!("{}{ext}", cut(stem, room, measure))
        }
        _ => cut(segment, limit, measure),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn key(text: &str) -> HostKey {
        HostKey::parse(text).unwrap()
    }

    /// Translation vectors shared with the TypeScript and Python SDKs'
    /// object-store helpers. `UPDATE_OBJECT_STORE_KEY_VECTORS=1` rewrites
    /// the expected keys from this implementation.
    #[test]
    fn object_store_translation_matches_the_shared_vectors() {
        const PATH: &str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packages/plugin-sdk/scripts/fixtures/object-store-keys.json"
        );
        let text = std::fs::read_to_string(PATH).unwrap();
        let mut cases: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        let update = std::env::var_os("UPDATE_OBJECT_STORE_KEY_VECTORS").is_some();
        for case in &mut cases {
            let host = key(case["key"].as_str().unwrap());
            let prefix = usize::try_from(case["prefixBytes"].as_u64().unwrap()).unwrap();
            let got = translate_for_object_store(&host, prefix);
            if update {
                case["expected"] = serde_json::Value::String(got);
            } else {
                assert_eq!(got, case["expected"].as_str().unwrap(), "{}", case["key"]);
            }
        }
        if update {
            let mut out = serde_json::to_string_pretty(&cases).unwrap();
            out.push('\n');
            std::fs::write(PATH, out).unwrap();
        }
    }

    #[test]
    fn host_keys_refuse_what_the_syntax_forbids() {
        for bad in [
            "", "/a", "a/", "a//b", "./a", "a/../b", "a\\b", "a\u{0}b", "a\nb",
        ] {
            assert!(HostKey::parse(bad).is_err(), "{bad:?}");
        }
        assert!(HostKey::parse(&"a".repeat(MAX_HOST_KEY_BYTES + 1)).is_err());
        let ok = key("Author: Name/Title?/Book.m4b");
        assert_eq!(
            ok.segments().collect::<Vec<_>>(),
            ["Author: Name", "Title?", "Book.m4b"]
        );
    }

    #[test]
    fn unix_paths_keep_every_character_and_cut_long_names() {
        assert_eq!(
            translate_for_filesystem(&key("A: B/C?.m4b"), FilesystemRules::Unix),
            "A: B/C?.m4b"
        );
        let long = format!("{}.m4b", "é".repeat(200));
        let out = translate_for_filesystem(&key(&long), FilesystemRules::Unix);
        assert!(out.len() <= MAX_FILESYSTEM_COMPONENT);
        assert!(out.ends_with(".m4b"));
    }

    #[test]
    fn windows_paths_replace_reserved_characters_and_names() {
        assert_eq!(
            translate_for_filesystem(
                &key("Who: \"Me\"/Why?. /CON.m4b/aux"),
                FilesystemRules::Windows
            ),
            "Who_ 'Me'/Why_/_CON.m4b/_aux"
        );
        assert_eq!(
            translate_for_filesystem(&key("..."), FilesystemRules::Windows),
            "_"
        );
        let long = format!("{}.m4b", "😀".repeat(200));
        let out = translate_for_filesystem(&key(&long), FilesystemRules::Windows);
        assert!(out.encode_utf16().count() <= MAX_FILESYSTEM_COMPONENT);
        assert!(out.ends_with(".m4b"));
    }

    #[test]
    fn object_keys_replace_avoided_characters() {
        assert_eq!(
            translate_for_object_store(&key("A [1] #2/50% ~x.m4b"), 0),
            "A (1) _2/50_ _x.m4b"
        );
    }

    #[test]
    fn object_keys_fit_the_budget_keeping_folders_and_extension() {
        let folder = "f".repeat(300);
        let file = format!("{}.m4b", "n".repeat(700));
        let host = key(&format!("{folder}/{file}"));
        let out = translate_for_object_store(&host, 100);
        assert!(out.len() <= MAX_OBJECT_KEY_BYTES - 100, "{}", out.len());
        assert!(out.starts_with(&folder));
        assert!(out.ends_with(".m4b"));

        // A prefix that leaves no room for the file name shortens folders.
        let out = translate_for_object_store(&host, 900);
        assert!(out.len() <= MAX_OBJECT_KEY_BYTES - 900, "{}", out.len());
        assert!(out.ends_with(".m4b"));
    }

    #[test]
    fn translation_is_stable_per_segment() {
        let a = translate_for_filesystem(&key("Long: Folder/one.m4b"), FilesystemRules::Windows);
        let b = translate_for_filesystem(&key("Long: Folder/two.jpg"), FilesystemRules::Windows);
        assert_eq!(a.split('/').next(), b.split('/').next());
    }
}
