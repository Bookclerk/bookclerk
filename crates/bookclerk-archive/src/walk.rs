//! The walk shared by every format: admitting names, copying bytes under the
//! limits, and recording what happened.

use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::format::{ArchiveFormat, SNIFF_BYTES};
use crate::limits::Limits;
use crate::path::{EntryPath, FoldedNames};
use crate::report::{ExtractReport, RefusalReason, StopReason, WrittenEntry};
use crate::sink::Sink;

/// Why an archive could not be walked at all.
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    /// The bytes start no format this crate expands.
    #[error("not a ZIP or tar archive")]
    NotArchive,
    /// The archive could not be read.
    #[error("reading the archive failed: {0}")]
    Io(#[from] io::Error),
}

/// Expands an archive through `sink` under `limits`.
///
/// The format is sniffed from the first bytes. Each entry's name is admitted
/// by [`EntryPath::admit`] and checked against the names written before it;
/// links, devices, encrypted entries, nested archives and system files are
/// refused; bytes are counted as they are written. A refused entry's partial
/// file is discarded. Reaching the entry count or the total stops the walk,
/// and the caller should then discard everything ([`StopReason`]).
///
/// # Arguments
///
/// * `reader` - The archive, positioned anywhere.
/// * `limits` - The caps for this extraction.
/// * `sink` - Where files go.
///
/// # Returns
///
/// The [`ExtractReport`]: files written, entries refused and why, and why
/// the walk stopped if it did.
///
/// # Errors
///
/// [`ExtractError::NotArchive`] when the format is not recognised, and
/// [`ExtractError::Io`] when the archive cannot be read before the walk
/// starts. A structure that breaks partway is reported as
/// [`StopReason::Corrupt`] instead.
pub fn extract<R: Read + Seek, S: Sink>(
    mut reader: R,
    limits: &Limits,
    sink: &mut S,
) -> Result<ExtractReport, ExtractError> {
    let archive_len = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(0))?;
    let mut head = Vec::with_capacity(SNIFF_BYTES);
    (&mut reader)
        .take(u64::try_from(SNIFF_BYTES).unwrap_or(u64::MAX))
        .read_to_end(&mut head)?;
    reader.seek(SeekFrom::Start(0))?;
    let format = ArchiveFormat::sniff(&head).ok_or(ExtractError::NotArchive)?;
    let mut walker = Walker::new(format, *limits);
    match format {
        ArchiveFormat::Zip => crate::zip_walk::walk(reader, &mut walker, sink),
        ArchiveFormat::Tar => crate::tar_walk::walk(reader, archive_len, false, &mut walker, sink),
        ArchiveFormat::TarGz => {
            crate::tar_walk::walk(reader, archive_len, true, &mut walker, sink);
        }
    }
    Ok(walker.report)
}

/// State one walk carries: the report so far, the names written, the limits.
pub(crate) struct Walker {
    /// What happened so far.
    pub report: ExtractReport,
    /// Names written so far.
    names: FoldedNames,
    /// The caps.
    pub limits: Limits,
    /// Entries seen so far, folders included.
    seen: u64,
}

/// What became of one entry's bytes.
pub(crate) enum Copied {
    /// The file was written.
    Written,
    /// The entry was refused; the walk goes on.
    Refused,
    /// The walk must stop.
    Stop,
}

/// Bytes copied per read.
const CHUNK: usize = 64 * 1024;

impl Walker {
    /// A walk of a `format` archive under `limits`.
    pub fn new(format: ArchiveFormat, limits: Limits) -> Self {
        Self {
            report: ExtractReport::new(format),
            names: FoldedNames::new(),
            limits,
            seen: 0,
        }
    }

    /// Counts one more entry; `false` (after recording the stop) when that
    /// is one past [`Limits::max_entries`].
    pub fn count_entry(&mut self) -> bool {
        self.seen = self.seen.saturating_add(1);
        if self.seen > u64::from(self.limits.max_entries) {
            self.report.stopped = Some(StopReason::TooManyEntries);
            return false;
        }
        true
    }

    /// Records why the walk stopped.
    pub fn stop(&mut self, reason: StopReason) {
        self.report.stopped.get_or_insert(reason);
    }

    /// Admits an entry's name, or records why it is refused.
    ///
    /// # Arguments
    ///
    /// * `raw` - The name's bytes, for the report.
    /// * `name` - The name decoded as the format defines, or `None` when it
    ///   is not valid text.
    pub fn admit(&mut self, raw: &[u8], name: Option<&str>) -> Option<EntryPath> {
        let admitted = match name {
            Some(name) => EntryPath::admit(name),
            None => EntryPath::admit_bytes(raw),
        };
        let path = match admitted {
            Ok(path) => path,
            Err(refusal) => {
                self.report.refuse(raw, RefusalReason::Path { refusal });
                return None;
            }
        };
        if path.is_system_file() {
            self.report.refuse(raw, RefusalReason::SystemFile);
            return None;
        }
        if let Err(clash) = self.names.check(&path) {
            self.report.refuse(raw, RefusalReason::Clash { clash });
            return None;
        }
        Some(path)
    }

    /// Copies one entry's bytes to `path` through `sink`.
    ///
    /// # Arguments
    ///
    /// * `sink` - Where the file goes.
    /// * `raw` - The entry's name bytes, for the report.
    /// * `path` - The admitted path.
    /// * `data` - The entry's decoded bytes.
    /// * `ratio_allowance` - Most bytes the entry may expand to by the ratio
    ///   rule; `u64::MAX` for an entry stored without compression.
    pub fn copy<S: Sink>(
        &mut self,
        sink: &mut S,
        raw: &[u8],
        path: &EntryPath,
        mut data: impl Read,
        ratio_allowance: u64,
    ) -> Copied {
        let total_left = self
            .limits
            .max_total_bytes
            .saturating_sub(self.report.total_bytes);
        let entry_cap = self.limits.max_entry_bytes.min(ratio_allowance);
        let mut writer = match sink.create(path) {
            Ok(writer) => writer,
            Err(_) => {
                self.report.refuse(raw, RefusalReason::WriteFailed);
                return Copied::Refused;
            }
        };
        let mut head = Vec::with_capacity(SNIFF_BYTES);
        let mut written: u64 = 0;
        let mut buf = vec![0u8; CHUNK];
        let outcome = loop {
            let n = match data.read(&mut buf) {
                Ok(0) => break None,
                Ok(n) => n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break Some(RefusalReason::Corrupt),
            };
            let chunk = &buf[..n];
            let next = written.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
            if next > total_left && total_left < entry_cap {
                drop(writer);
                sink.discard(path);
                self.stop(StopReason::TotalExceeded);
                return Copied::Stop;
            }
            if next > entry_cap {
                break Some(if entry_cap < self.limits.max_entry_bytes {
                    RefusalReason::RatioExceeded
                } else {
                    RefusalReason::EntryTooLarge
                });
            }
            if head.len() < SNIFF_BYTES {
                let take = (SNIFF_BYTES - head.len()).min(chunk.len());
                head.extend_from_slice(&chunk[..take]);
            }
            if writer.write_all(chunk).is_err() {
                break Some(RefusalReason::WriteFailed);
            }
            written = next;
        };
        let outcome = outcome.or_else(|| {
            if ArchiveFormat::looks_like_archive(&head) {
                Some(RefusalReason::NestedArchive)
            } else if writer.flush().is_err() {
                Some(RefusalReason::WriteFailed)
            } else {
                None
            }
        });
        drop(writer);
        match outcome {
            Some(reason) => {
                sink.discard(path);
                self.report.refuse(raw, reason);
                Copied::Refused
            }
            None => {
                // `check` passed in `admit` and no other file was written
                // since, so this cannot clash.
                if let Err(clash) = self.names.insert(path) {
                    sink.discard(path);
                    self.report.refuse(raw, RefusalReason::Clash { clash });
                    return Copied::Refused;
                }
                self.report.total_bytes = self.report.total_bytes.saturating_add(written);
                self.report.written.push(WrittenEntry {
                    path: path.clone(),
                    bytes: written,
                });
                Copied::Written
            }
        }
    }
}
