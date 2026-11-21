//! Bounded extraction of the archives people upload.
//!
//! An uploaded ZIP or tar file is untrusted input: its entry names may try to
//! escape the output folder, its sizes may lie, and its contents may expand
//! without end. This crate decides what an archive may write and stops it
//! when it overreaches. It never opens a file itself:
//!
//! - [`EntryPath::admit`] turns a raw entry name into a relative path that
//!   cannot leave the output folder, or refuses it.
//! - [`extract`] walks an archive read from any `Read + Seek` source and
//!   writes each admitted entry through a [`Sink`], counting the bytes it
//!   actually writes against [`Limits`] rather than trusting the archive's
//!   headers.
//! - [`ExtractReport::validate`] re-checks a report that crossed a process
//!   boundary, because the code that produced it parsed untrusted bytes.
//!
//! The same code runs in the confined media worker's `extract` job, in the
//! `archive_extract` fuzz target (over an in-memory sink) and in the unit
//! tests. Nested archives are never expanded, links and devices are never
//! created, and encrypted entries are refused.

#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures;
mod format;
mod limits;
mod path;
mod report;
mod sink;
mod tar_walk;
mod walk;
mod zip_walk;

pub use format::{ArchiveFormat, SNIFF_BYTES};
pub use limits::Limits;
pub use path::{EntryPath, FoldedNames, NameClash, PathRefusal, MAX_DEPTH, MAX_PATH_BYTES};
pub use report::{
    ExtractReport, RefusalReason, RefusedEntry, ReportError, StopReason, WrittenEntry,
    MAX_REFUSED_NAME_CHARS,
};
pub use sink::{DirSink, MemorySink, Sink};
pub use walk::{extract, ExtractError};
