//! Walking a ZIP archive.

use std::io::{Read, Seek};

use zip::{CompressionMethod, ZipArchive};

use crate::report::{RefusalReason, StopReason};
use crate::sink::Sink;
use crate::walk::{Copied, Walker};

/// `S_IFMT`: the file-type bits of a Unix mode.
const S_IFMT: u32 = 0o170_000;
/// `S_IFREG`: a regular file.
const S_IFREG: u32 = 0o100_000;
/// `S_IFDIR`: a directory.
const S_IFDIR: u32 = 0o040_000;
/// `S_IFLNK`: a symbolic link.
const S_IFLNK: u32 = 0o120_000;

/// What the first pass learned about one entry without decoding it.
#[derive(Debug, Clone, Copy)]
struct Span {
    /// Where its local header starts.
    start: u64,
    /// Where its compressed data ends.
    end: u64,
}

/// Walks a ZIP archive: one pass over the central directory to count
/// entries and find overlapping data, then one pass that writes.
pub(crate) fn walk<R: Read + Seek, S: Sink>(reader: R, walker: &mut Walker, sink: &mut S) {
    let mut archive = match ZipArchive::new(reader) {
        Ok(archive) => archive,
        Err(_) => {
            walker.stop(StopReason::Corrupt);
            return;
        }
    };
    let count = archive.len();
    if u64::try_from(count).map_or(true, |n| n > u64::from(walker.limits.max_entries)) {
        walker.stop(StopReason::TooManyEntries);
        return;
    }
    let overlapping = overlaps(&mut archive);
    for index in 0..count {
        if !walker.count_entry() {
            return;
        }
        let (raw, name, kind, compressed, encrypted, method_ok) = match archive.by_index_raw(index)
        {
            Ok(file) => (
                file.name_raw().to_vec(),
                file.name().ok().map(|name| name.into_owned()),
                kind_of(file.is_dir(), file.is_symlink(), file.unix_mode()),
                file.compressed_size(),
                file.encrypted(),
                matches!(
                    file.compression(),
                    CompressionMethod::Stored
                        | CompressionMethod::Deflated
                        | CompressionMethod::Deflate64
                ),
            ),
            Err(_) => {
                walker.report.refuse(b"", RefusalReason::Corrupt);
                continue;
            }
        };
        match kind {
            Kind::Dir => continue,
            Kind::Link => {
                walker.report.refuse(&raw, RefusalReason::Link);
                continue;
            }
            Kind::Special => {
                walker.report.refuse(&raw, RefusalReason::Special);
                continue;
            }
            Kind::File => {}
        }
        if encrypted {
            walker.report.refuse(&raw, RefusalReason::Encrypted);
            continue;
        }
        if !method_ok {
            walker.report.refuse(&raw, RefusalReason::Compression);
            continue;
        }
        if overlapping.get(index).copied().unwrap_or(true) {
            walker.report.refuse(&raw, RefusalReason::Overlap);
            continue;
        }
        let Some(path) = walker.admit(&raw, name.as_deref()) else {
            continue;
        };
        let file = match archive.by_index(index) {
            Ok(file) => file,
            Err(_) => {
                walker.report.refuse(&raw, RefusalReason::Corrupt);
                continue;
            }
        };
        let allowance = walker.limits.ratio_allowance(compressed);
        match walker.copy(sink, &raw, &path, file, allowance) {
            Copied::Written | Copied::Refused => {}
            Copied::Stop => return,
        }
    }
}

/// What kind of thing a ZIP entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A regular file.
    File,
    /// A folder.
    Dir,
    /// A symbolic link.
    Link,
    /// A device, FIFO or socket.
    Special,
}

/// Classifies an entry from its flags and Unix mode, when it has one.
fn kind_of(is_dir: bool, is_symlink: bool, mode: Option<u32>) -> Kind {
    if is_symlink {
        return Kind::Link;
    }
    match mode.map(|mode| mode & S_IFMT) {
        Some(S_IFLNK) => Kind::Link,
        Some(S_IFDIR) => Kind::Dir,
        Some(S_IFREG) | Some(0) | None => {
            if is_dir {
                Kind::Dir
            } else {
                Kind::File
            }
        }
        Some(_) => Kind::Special,
    }
}

/// Marks every entry whose local header and data overlap another entry's.
///
/// Entries the first pass cannot place are marked too.
fn overlaps<R: Read + Seek>(archive: &mut ZipArchive<R>) -> Vec<bool> {
    let count = archive.len();
    let mut spans: Vec<Option<Span>> = Vec::with_capacity(count);
    for index in 0..count {
        let span = archive.by_index_raw(index).ok().and_then(|file| {
            let start = file.header_start();
            let data = file.data_start()?;
            let end = data.checked_add(file.compressed_size())?;
            (start <= data).then_some(Span { start, end })
        });
        spans.push(span);
    }
    let mut marked: Vec<bool> = spans.iter().map(Option::is_none).collect();
    let mut order: Vec<usize> = (0..count).filter(|&i| spans[i].is_some()).collect();
    order.sort_by_key(|&i| spans[i].map(|span| (span.start, span.end)));
    let mut furthest: Option<(u64, usize)> = None;
    for &index in &order {
        let Some(span) = spans[index] else {
            continue;
        };
        if let Some((end, owner)) = furthest {
            if span.start < end {
                marked[index] = true;
                marked[owner] = true;
            }
            if span.end > end {
                furthest = Some((span.end, index));
            }
        } else {
            furthest = Some((span.end, index));
        }
    }
    marked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_entries_by_mode() {
        assert_eq!(kind_of(false, false, None), Kind::File);
        assert_eq!(kind_of(true, false, None), Kind::Dir);
        assert_eq!(kind_of(false, true, None), Kind::Link);
        assert_eq!(kind_of(false, false, Some(0o100_644)), Kind::File);
        assert_eq!(kind_of(false, false, Some(0o120_777)), Kind::Link);
        assert_eq!(kind_of(false, false, Some(0o020_644)), Kind::Special);
        assert_eq!(kind_of(false, false, Some(0o010_644)), Kind::Special);
    }
}
