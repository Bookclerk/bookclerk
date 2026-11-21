//! What an extraction wrote and refused, and the checks the host runs on a
//! report another process produced.

use serde::{Deserialize, Serialize};

use crate::format::ArchiveFormat;
use crate::limits::Limits;
use crate::path::{EntryPath, FoldedNames, NameClash, PathRefusal};

/// Longest name kept for a refused entry, in characters. Longer names are
/// cut; the name is only shown to people, never used as a path.
pub const MAX_REFUSED_NAME_CHARS: usize = 256;

/// One file the extraction wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WrittenEntry {
    /// Where it was written, relative to the output folder.
    pub path: EntryPath,
    /// How many bytes were written.
    pub bytes: u64,
}

/// One entry the extraction did not write, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RefusedEntry {
    /// The entry's name as the archive spells it, for people to read:
    /// invalid UTF-8 and control characters replaced, cut to
    /// [`MAX_REFUSED_NAME_CHARS`].
    pub name: String,
    /// Why it was refused.
    pub reason: RefusalReason,
}

/// Why one entry was not written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RefusalReason {
    /// Its name breaks the path policy.
    Path {
        /// The rule it breaks.
        refusal: PathRefusal,
    },
    /// Its name clashes with an earlier entry.
    Clash {
        /// How it clashes.
        clash: NameClash,
    },
    /// It is a symbolic or hard link.
    Link,
    /// It is a device, a FIFO or another special file.
    Special,
    /// It is encrypted.
    Encrypted,
    /// It uses a compression method this crate does not decode.
    Compression,
    /// It is itself an archive; nested archives are not expanded.
    NestedArchive,
    /// It would write more than [`Limits::max_entry_bytes`].
    EntryTooLarge,
    /// It expands past [`Limits::max_ratio`].
    RatioExceeded,
    /// Its data overlaps another entry's (a ZIP that reuses one stream for
    /// many names).
    Overlap,
    /// It is an operating system's bookkeeping file, such as `__MACOSX/`.
    SystemFile,
    /// Its data could not be decoded or failed its checksum.
    Corrupt,
    /// The output file could not be created or written.
    WriteFailed,
}

/// Why an extraction stopped before the end of the archive.
///
/// A stopped extraction's files are kept on disk only until the host reads
/// the report; the host discards the whole archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The archive lists more than [`Limits::max_entries`] entries.
    TooManyEntries,
    /// Writing went past [`Limits::max_total_bytes`], or a gzip stream past
    /// its ratio allowance.
    TotalExceeded,
    /// The archive's structure broke partway through.
    Corrupt,
}

/// What one extraction did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtractReport {
    /// The format the archive was walked as.
    pub format: ArchiveFormat,
    /// The files written, in archive order.
    pub written: Vec<WrittenEntry>,
    /// The entries refused, in archive order. Folders are not listed.
    pub refused: Vec<RefusedEntry>,
    /// Why the walk stopped early, if it did.
    pub stopped: Option<StopReason>,
    /// Bytes written in all; the sum of [`WrittenEntry::bytes`].
    pub total_bytes: u64,
}

/// A report that breaks the limits it was produced under.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReportError {
    /// More entries than [`Limits::max_entries`].
    #[error("the report lists more entries than the limit")]
    TooManyEntries,
    /// One file larger than [`Limits::max_entry_bytes`].
    #[error("{0} is larger than the entry limit")]
    EntryTooLarge(String),
    /// The total is not the sum of the files, or is past the limit.
    #[error("the report's total is wrong or past the limit")]
    Total,
    /// Two files clash.
    #[error("{0} clashes with another written file")]
    Clash(String),
    /// A refused entry's name is too long or holds a control character.
    #[error("a refused entry's name is malformed")]
    RefusedName,
}

impl ExtractReport {
    /// An empty report for an archive of `format`.
    ///
    /// # Arguments
    ///
    /// * `format` - The format being walked.
    #[must_use]
    pub fn new(format: ArchiveFormat) -> Self {
        Self {
            format,
            written: Vec::new(),
            refused: Vec::new(),
            stopped: None,
            total_bytes: 0,
        }
    }

    /// Records a refused entry, cleaning its name for display.
    ///
    /// # Arguments
    ///
    /// * `raw_name` - The entry's name bytes.
    /// * `reason` - Why it was refused.
    pub fn refuse(&mut self, raw_name: &[u8], reason: RefusalReason) {
        self.refused.push(RefusedEntry {
            name: display_name(raw_name),
            reason,
        });
    }

    /// Checks a report produced by another process against the limits the
    /// job ran under.
    ///
    /// Paths were already admitted when the report was deserialised; this
    /// adds the checks that span entries.
    ///
    /// # Arguments
    ///
    /// * `limits` - The limits sent with the job.
    ///
    /// # Errors
    ///
    /// The first [`ReportError`] found.
    pub fn validate(&self, limits: &Limits) -> Result<(), ReportError> {
        let listed = self.written.len().saturating_add(self.refused.len());
        if u64::try_from(listed).map_or(true, |n| n > u64::from(limits.max_entries)) {
            return Err(ReportError::TooManyEntries);
        }
        let mut names = FoldedNames::new();
        let mut sum: u64 = 0;
        for entry in &self.written {
            if entry.bytes > limits.max_entry_bytes {
                return Err(ReportError::EntryTooLarge(entry.path.to_string()));
            }
            names
                .insert(&entry.path)
                .map_err(|_| ReportError::Clash(entry.path.to_string()))?;
            sum = sum.checked_add(entry.bytes).ok_or(ReportError::Total)?;
        }
        if sum != self.total_bytes || sum > limits.max_total_bytes {
            return Err(ReportError::Total);
        }
        for entry in &self.refused {
            if entry.name.chars().count() > MAX_REFUSED_NAME_CHARS
                || entry.name.chars().any(char::is_control)
            {
                return Err(ReportError::RefusedName);
            }
        }
        Ok(())
    }
}

/// An entry name made safe to show: lossy UTF-8, control characters as `?`,
/// at most [`MAX_REFUSED_NAME_CHARS`] characters.
fn display_name(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .take(MAX_REFUSED_NAME_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(path: &str, bytes: u64) -> WrittenEntry {
        WrittenEntry {
            path: EntryPath::admit(path).unwrap(),
            bytes,
        }
    }

    #[test]
    fn a_consistent_report_validates() {
        let mut report = ExtractReport::new(ArchiveFormat::Zip);
        report.written = vec![written("a/b.mp3", 10), written("a/c.mp3", 5)];
        report.total_bytes = 15;
        report.refuse(
            b"../evil\n",
            RefusalReason::Path {
                refusal: PathRefusal::ParentDir,
            },
        );
        assert_eq!(report.refused[0].name, "../evil?");
        report.validate(&Limits::UPLOAD_DEFAULTS).unwrap();
        let json = serde_json::to_string(&report).unwrap();
        assert_eq!(
            serde_json::from_str::<ExtractReport>(&json).unwrap(),
            report
        );
    }

    #[test]
    fn inconsistent_reports_are_refused() {
        let limits = Limits {
            max_entries: 2,
            max_total_bytes: 20,
            max_entry_bytes: 12,
            ..Limits::UPLOAD_DEFAULTS
        };
        let mut report = ExtractReport::new(ArchiveFormat::Tar);
        report.written = vec![written("a", 10), written("A", 1)];
        report.total_bytes = 11;
        assert!(matches!(
            report.validate(&limits),
            Err(ReportError::Clash(_))
        ));
        report.written = vec![written("a", 13)];
        report.total_bytes = 13;
        assert!(matches!(
            report.validate(&limits),
            Err(ReportError::EntryTooLarge(_))
        ));
        report.written = vec![written("a", 10)];
        report.total_bytes = 9;
        assert_eq!(report.validate(&limits), Err(ReportError::Total));
        report.total_bytes = 10;
        report.refuse(b"x", RefusalReason::Link);
        report.refuse(b"y", RefusalReason::Link);
        assert_eq!(report.validate(&limits), Err(ReportError::TooManyEntries));
    }

    #[test]
    fn non_canonical_paths_do_not_deserialise() {
        let json = r#"{"format":"zip","written":[{"path":"../x","bytes":1}],"refused":[],"stopped":null,"totalBytes":1}"#;
        assert!(serde_json::from_str::<ExtractReport>(json).is_err());
    }
}
