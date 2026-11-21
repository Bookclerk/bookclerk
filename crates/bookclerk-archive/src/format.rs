//! Recognising an archive from its first bytes.

use serde::{Deserialize, Serialize};

/// The archive formats Bookclerk expands.
///
/// The format is always decided from the bytes ([`ArchiveFormat::sniff`]),
/// never from the file name, so a renamed file cannot pick a decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFormat {
    /// A ZIP archive (stored, Deflate or Deflate64 entries; Zip64 sizes).
    Zip,
    /// An uncompressed POSIX, GNU or PAX tar archive.
    Tar,
    /// A tar archive inside one gzip stream.
    TarGz,
}

/// Bytes [`ArchiveFormat::sniff`] needs to recognise every format: the tar
/// magic sits at offset 257.
pub const SNIFF_BYTES: usize = 265;

impl ArchiveFormat {
    /// Recognises an archive from its first bytes.
    ///
    /// # Arguments
    ///
    /// * `head` - The file's first bytes; [`SNIFF_BYTES`] is enough for every
    ///   format, fewer may miss a tar archive.
    ///
    /// # Returns
    ///
    /// The format, or `None` when the bytes start no format this crate
    /// expands. A gzip stream is reported as [`ArchiveFormat::TarGz`]; whether
    /// it really holds a tar archive shows when it is walked.
    #[must_use]
    pub fn sniff(head: &[u8]) -> Option<Self> {
        // A local file header, or the end-of-central-directory record of an
        // empty archive.
        if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
            return Some(Self::Zip);
        }
        if head.starts_with(&[0x1f, 0x8b]) {
            return Some(Self::TarGz);
        }
        match head.get(257..262) {
            Some(b"ustar") => Some(Self::Tar),
            _ => None,
        }
    }

    /// Whether bytes look like any archive or compressed container, this
    /// crate's formats or others (7z, RAR, xz, bzip2, zstd).
    ///
    /// Used to refuse nested archives: an entry that is itself an archive is
    /// not expanded and not kept.
    ///
    /// # Arguments
    ///
    /// * `head` - The entry's first bytes.
    #[must_use]
    pub fn looks_like_archive(head: &[u8]) -> bool {
        const OTHERS: &[&[u8]] = &[
            b"7z\xbc\xaf\x27\x1c",
            b"Rar!\x1a\x07",
            b"\xfd7zXZ\x00",
            b"BZh",
            b"\x28\xb5\x2f\xfd",
        ];
        Self::sniff(head).is_some() || OTHERS.iter().any(|magic| head.starts_with(magic))
    }

    /// The name used in logs and replies.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::Tar => "tar",
            Self::TarGz => "tar_gz",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_each_format_from_its_magic() {
        assert_eq!(
            ArchiveFormat::sniff(b"PK\x03\x04rest"),
            Some(ArchiveFormat::Zip)
        );
        assert_eq!(
            ArchiveFormat::sniff(b"PK\x05\x06"),
            Some(ArchiveFormat::Zip)
        );
        assert_eq!(
            ArchiveFormat::sniff(&[0x1f, 0x8b, 8]),
            Some(ArchiveFormat::TarGz)
        );
        let mut tar = vec![0u8; 512];
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(ArchiveFormat::sniff(&tar), Some(ArchiveFormat::Tar));
        assert_eq!(ArchiveFormat::sniff(b"ID3\x04"), None);
        assert_eq!(ArchiveFormat::sniff(b""), None);
    }

    #[test]
    fn other_containers_count_as_archives() {
        assert!(ArchiveFormat::looks_like_archive(b"Rar!\x1a\x07\x01"));
        assert!(ArchiveFormat::looks_like_archive(b"7z\xbc\xaf\x27\x1c"));
        assert!(ArchiveFormat::looks_like_archive(b"PK\x03\x04"));
        assert!(!ArchiveFormat::looks_like_archive(b"fLaC"));
    }
}
