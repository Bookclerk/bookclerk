//! Walking a tar archive, plain or inside one gzip stream.

use std::cell::Cell;
use std::io::{self, Read};
use std::rc::Rc;

use flate2::read::GzDecoder;
use tar::{Archive, EntryType};

use crate::report::{RefusalReason, StopReason};
use crate::sink::Sink;
use crate::walk::{Copied, Walker};

/// A reader that fails once it has handed out `cap` bytes, and says so.
///
/// It bounds the bytes a tar walk reads, including the bytes the tar reader
/// skips over for a refused entry, so a gzip stream that expands without end
/// or a header that claims an enormous size cannot keep the walk busy.
struct Capped<R> {
    /// The source.
    inner: R,
    /// Bytes still allowed.
    left: u64,
    /// Set once the cap is reached, shared with the walk.
    hit: Rc<Cell<bool>>,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            self.hit.set(true);
            return Err(io::Error::other("archive read cap reached"));
        }
        let want = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..want])?;
        self.left = self
            .left
            .saturating_sub(u64::try_from(n).unwrap_or(u64::MAX));
        Ok(n)
    }
}

/// Walks a tar archive.
///
/// # Arguments
///
/// * `reader` - The archive file.
/// * `archive_len` - Its size in bytes.
/// * `gzip` - Whether it is one gzip stream around the tar.
/// * `walker` - The walk's state.
/// * `sink` - Where files go.
pub(crate) fn walk<R: Read, S: Sink>(
    reader: R,
    archive_len: u64,
    gzip: bool,
    walker: &mut Walker,
    sink: &mut S,
) {
    // Plain tar cannot read past its file. A gzip stream may expand to the
    // ratio allowance, plus room for the headers of every entry.
    let header_room = u64::from(walker.limits.max_entries).saturating_mul(3 * 512);
    let hit = Rc::new(Cell::new(false));
    if gzip {
        let cap = walker
            .limits
            .ratio_allowance(archive_len)
            .min(walker.limits.max_total_bytes.saturating_add(header_room));
        let decoded = Capped {
            inner: GzDecoder::new(reader),
            left: cap,
            hit: hit.clone(),
        };
        walk_entries(Archive::new(decoded), &hit, walker, sink);
    } else {
        let plain = Capped {
            inner: reader,
            left: archive_len,
            hit: hit.clone(),
        };
        walk_entries(Archive::new(plain), &hit, walker, sink);
    }
}

/// Walks the entries of an opened tar stream.
fn walk_entries<R: Read, S: Sink>(
    mut archive: Archive<R>,
    hit: &Rc<Cell<bool>>,
    walker: &mut Walker,
    sink: &mut S,
) {
    let entries = match archive.entries() {
        Ok(entries) => entries,
        Err(_) => {
            walker.stop(StopReason::Corrupt);
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                walker.stop(if hit.get() {
                    StopReason::TotalExceeded
                } else {
                    StopReason::Corrupt
                });
                return;
            }
        };
        if !walker.count_entry() {
            return;
        }
        let raw = entry.path_bytes().into_owned();
        match entry.header().entry_type() {
            EntryType::Regular | EntryType::Continuous => {}
            EntryType::Directory => continue,
            EntryType::Symlink | EntryType::Link => {
                walker.report.refuse(&raw, RefusalReason::Link);
                continue;
            }
            EntryType::Char | EntryType::Block | EntryType::Fifo => {
                walker.report.refuse(&raw, RefusalReason::Special);
                continue;
            }
            // Sparse files and anything else this walk does not lay out.
            _ => {
                walker.report.refuse(&raw, RefusalReason::Compression);
                continue;
            }
        }
        let Some(path) = walker.admit(&raw, None) else {
            continue;
        };
        // Tar stores entries uncompressed; the gzip stream as a whole is
        // bounded by the cap on the reader.
        match walker.copy(sink, &raw, &path, entry, u64::MAX) {
            Copied::Written | Copied::Refused => {}
            Copied::Stop => return,
        }
        if hit.get() {
            walker.stop(StopReason::TotalExceeded);
            return;
        }
    }
}
