//! The path policy for names that come from untrusted input: archive entry
//! names and the relative paths of an upload manifest.

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization as _;

/// Longest admitted path, in bytes of UTF-8.
pub const MAX_PATH_BYTES: usize = 1024;

/// Longest admitted path component, in bytes of UTF-8 (the common
/// filesystem limit).
pub const MAX_COMPONENT_BYTES: usize = 255;

/// Most components an admitted path may have.
pub const MAX_DEPTH: usize = 32;

/// Names Windows reserves for devices, with or without an extension.
const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// A relative path that stays inside the folder it is joined to.
///
/// Components are separated by `/`, none is empty, `.` or `..`, and none
/// holds a control character, a `:` or a `\`. The path is valid UTF-8, at
/// most [`MAX_PATH_BYTES`] long and at most [`MAX_DEPTH`] deep, and no
/// component is a Windows device name or ends in a dot or a space, so the
/// same path is safe on every filesystem a destination may use.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct EntryPath(String);

/// Why a name was not admitted as an [`EntryPath`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum PathRefusal {
    /// The name is not valid UTF-8.
    #[error("the name is not UTF-8")]
    NotUtf8,
    /// Nothing is left once empty and `.` components are dropped (a
    /// directory entry such as `./`).
    #[error("the name is empty")]
    Empty,
    /// The name starts at a root (`/`, `\`).
    #[error("the name is absolute")]
    Absolute,
    /// The name holds a `:`, which names a drive, a stream or a device on
    /// Windows.
    #[error("the name holds a colon")]
    Colon,
    /// A component is `..`.
    #[error("the name climbs out with ..")]
    ParentDir,
    /// The name holds a control character (including NUL).
    #[error("the name holds a control character")]
    Control,
    /// A component is a Windows device name or ends in a dot or a space.
    #[error("a component is reserved on Windows")]
    Reserved,
    /// The name or one of its components is too long.
    #[error("the name is too long")]
    TooLong,
    /// The name has more than [`MAX_DEPTH`] components.
    #[error("the name is too deep")]
    TooDeep,
}

impl EntryPath {
    /// Admits a name as a relative path, or says why it cannot be one.
    ///
    /// `\` is read as a separator (ZIP tools on Windows write it), empty and
    /// `.` components are dropped, and everything else in the policy above is
    /// refused rather than repaired.
    ///
    /// # Arguments
    ///
    /// * `raw` - The name as the archive or manifest spells it.
    ///
    /// # Errors
    ///
    /// The [`PathRefusal`] for the first rule the name breaks.
    pub fn admit(raw: &str) -> Result<Self, PathRefusal> {
        if raw.chars().any(char::is_control) {
            return Err(PathRefusal::Control);
        }
        let unified = raw.replace('\\', "/");
        if unified.starts_with('/') {
            return Err(PathRefusal::Absolute);
        }
        if unified.contains(':') {
            return Err(PathRefusal::Colon);
        }
        let mut parts = Vec::new();
        for part in unified.split('/') {
            match part {
                "" | "." => {}
                ".." => return Err(PathRefusal::ParentDir),
                part => {
                    if part.len() > MAX_COMPONENT_BYTES {
                        return Err(PathRefusal::TooLong);
                    }
                    if is_reserved(part) {
                        return Err(PathRefusal::Reserved);
                    }
                    parts.push(part);
                }
            }
        }
        if parts.is_empty() {
            return Err(PathRefusal::Empty);
        }
        if parts.len() > MAX_DEPTH {
            return Err(PathRefusal::TooDeep);
        }
        let joined = parts.join("/");
        if joined.len() > MAX_PATH_BYTES {
            return Err(PathRefusal::TooLong);
        }
        Ok(Self(joined))
    }

    /// Admits a name given as bytes, which must be UTF-8.
    ///
    /// # Arguments
    ///
    /// * `raw` - The name's bytes, as a tar header carries them.
    ///
    /// # Errors
    ///
    /// [`PathRefusal::NotUtf8`], or what [`EntryPath::admit`] refuses.
    pub fn admit_bytes(raw: &[u8]) -> Result<Self, PathRefusal> {
        std::str::from_utf8(raw)
            .map_err(|_| PathRefusal::NotUtf8)
            .and_then(Self::admit)
    }

    /// The path, components separated by `/`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path's components, outermost first.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// The last component.
    #[must_use]
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// The key two paths clash on: NFC-normalised and lower-cased, so names
    /// a case-insensitive or normalising filesystem would merge share it.
    #[must_use]
    pub fn fold_key(&self) -> String {
        fold(&self.0)
    }

    /// Whether this is an operating system's bookkeeping file rather than
    /// content: macOS resource forks (`__MACOSX/`, `._name`), `.DS_Store`,
    /// and Windows' `Thumbs.db` and `desktop.ini`.
    #[must_use]
    pub fn is_system_file(&self) -> bool {
        let name = self.file_name();
        self.components().next() == Some("__MACOSX")
            || name.starts_with("._")
            || name.eq_ignore_ascii_case(".DS_Store")
            || name.eq_ignore_ascii_case("Thumbs.db")
            || name.eq_ignore_ascii_case("desktop.ini")
    }
}

impl<'de> Deserialize<'de> for EntryPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let path = Self::admit(&raw).map_err(serde::de::Error::custom)?;
        // A path that admission would respell (a `\`, a `./`) is not the
        // canonical form another process wrote, so it is refused too.
        if path.0 != raw {
            return Err(serde::de::Error::custom("path is not in canonical form"));
        }
        Ok(path)
    }
}

impl fmt::Display for EntryPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether a component is a Windows device name (with or without an
/// extension) or ends in a dot or a space.
fn is_reserved(part: &str) -> bool {
    if part.ends_with('.') || part.ends_with(' ') {
        return true;
    }
    let stem = part.split('.').next().unwrap_or(part).trim_end();
    RESERVED.iter().any(|name| stem.eq_ignore_ascii_case(name))
}

/// NFC, then lower case.
fn fold(text: &str) -> String {
    text.nfc().collect::<String>().to_lowercase()
}

/// Why a path cannot join a [`FoldedNames`] set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum NameClash {
    /// Another file already has the same folded name.
    #[error("another entry has the same name")]
    Duplicate,
    /// The path needs a folder where a file already is, or a file where a
    /// folder already is.
    #[error("a file and a folder share a name")]
    FileFolder,
}

/// The files written so far, by folded name, and the folders they imply.
///
/// [`FoldedNames::insert`] refuses a path that would overwrite an earlier
/// file on any filesystem, or needs a folder where a file is.
#[derive(Debug, Default, Clone)]
pub struct FoldedNames {
    /// Folded keys of the files.
    files: HashSet<String>,
    /// Folded keys of every folder a file sits in.
    folders: HashSet<String>,
}

impl FoldedNames {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks a file path against the set without adding it.
    ///
    /// # Arguments
    ///
    /// * `path` - The file about to be written.
    ///
    /// # Errors
    ///
    /// The clash, when the path cannot be written beside the others.
    pub fn check(&self, path: &EntryPath) -> Result<(), NameClash> {
        let key = path.fold_key();
        if self.files.contains(&key) {
            return Err(NameClash::Duplicate);
        }
        if self.folders.contains(&key) {
            return Err(NameClash::FileFolder);
        }
        let mut prefix = String::new();
        let mut parts = key.split('/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                break;
            }
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            if self.files.contains(&prefix) {
                return Err(NameClash::FileFolder);
            }
        }
        Ok(())
    }

    /// Adds a file path, after [`FoldedNames::check`].
    ///
    /// # Arguments
    ///
    /// * `path` - The file being written.
    ///
    /// # Errors
    ///
    /// The clash, when the path cannot be written beside the others; the set
    /// is then unchanged.
    pub fn insert(&mut self, path: &EntryPath) -> Result<(), NameClash> {
        self.check(path)?;
        let key = path.fold_key();
        let mut prefix = String::new();
        let mut parts = key.split('/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                break;
            }
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            self.folders.insert(prefix.clone());
        }
        self.files.insert(key);
        Ok(())
    }

    /// How many files the set holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the set holds no file.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admits_ordinary_relative_paths() {
        let path = EntryPath::admit("Book/Disc 1/01 Chapter.mp3").unwrap();
        assert_eq!(path.as_str(), "Book/Disc 1/01 Chapter.mp3");
        assert_eq!(path.file_name(), "01 Chapter.mp3");
        assert_eq!(
            EntryPath::admit("./Book\\cover.jpg").unwrap().as_str(),
            "Book/cover.jpg"
        );
        assert_eq!(EntryPath::admit("a//b").unwrap().as_str(), "a/b");
    }

    #[test]
    fn refuses_every_escape() {
        let cases = [
            ("/etc/passwd", PathRefusal::Absolute),
            ("\\server\\share", PathRefusal::Absolute),
            ("C:/Windows", PathRefusal::Colon),
            ("file.txt:stream", PathRefusal::Colon),
            ("../up", PathRefusal::ParentDir),
            ("a/../../up", PathRefusal::ParentDir),
            ("a\\..\\up", PathRefusal::ParentDir),
            ("nul\0byte", PathRefusal::Control),
            ("line\nbreak", PathRefusal::Control),
            ("CON", PathRefusal::Reserved),
            ("dir/lpt1.txt", PathRefusal::Reserved),
            ("trailing.", PathRefusal::Reserved),
            ("trailing ", PathRefusal::Reserved),
            ("", PathRefusal::Empty),
            ("./", PathRefusal::Empty),
        ];
        for (raw, refusal) in cases {
            assert_eq!(EntryPath::admit(raw), Err(refusal), "{raw:?}");
        }
        assert_eq!(
            EntryPath::admit(&"a/".repeat(MAX_DEPTH + 1)),
            Err(PathRefusal::TooDeep)
        );
        assert_eq!(
            EntryPath::admit(&"x".repeat(MAX_COMPONENT_BYTES + 1)),
            Err(PathRefusal::TooLong)
        );
        let long = vec!["y".repeat(200); 6].join("/");
        assert_eq!(EntryPath::admit(&long), Err(PathRefusal::TooLong));
        assert_eq!(
            EntryPath::admit_bytes(b"\xff\xfe"),
            Err(PathRefusal::NotUtf8)
        );
    }

    #[test]
    fn clashes_fold_case_and_normalisation() {
        let mut names = FoldedNames::new();
        names
            .insert(&EntryPath::admit("Book/Track.mp3").unwrap())
            .unwrap();
        assert_eq!(
            names.insert(&EntryPath::admit("book/TRACK.MP3").unwrap()),
            Err(NameClash::Duplicate)
        );
        // "é" precomposed and decomposed.
        names
            .insert(&EntryPath::admit("caf\u{e9}.txt").unwrap())
            .unwrap();
        assert_eq!(
            names.insert(&EntryPath::admit("cafe\u{301}.txt").unwrap()),
            Err(NameClash::Duplicate)
        );
        assert_eq!(
            names.insert(&EntryPath::admit("Book/Track.mp3/inner").unwrap()),
            Err(NameClash::FileFolder)
        );
        assert_eq!(
            names.insert(&EntryPath::admit("BOOK").unwrap()),
            Err(NameClash::FileFolder)
        );
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn spots_system_files() {
        for raw in ["__MACOSX/a/._b", "a/._b.mp3", "a/.DS_Store", "Thumbs.db"] {
            assert!(EntryPath::admit(raw).unwrap().is_system_file(), "{raw}");
        }
        assert!(!EntryPath::admit("a/b.mp3").unwrap().is_system_file());
    }

    #[test]
    fn deserialising_refuses_non_canonical_paths() {
        assert!(serde_json::from_str::<EntryPath>("\"a/b\"").is_ok());
        assert!(serde_json::from_str::<EntryPath>("\"./a\"").is_err());
        assert!(serde_json::from_str::<EntryPath>("\"../a\"").is_err());
    }
}
