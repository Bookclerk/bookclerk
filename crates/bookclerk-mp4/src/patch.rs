//! Swap a file's `moov` without moving its media.
//!
//! Everything an audiobook gains after its samples are written — tags, a cover,
//! a chapter track — lands in `moov`, and `moov` sits in front of the media in a
//! faststart file. Growing it the obvious way pushes the media along, which
//! means rewriting the whole book and correcting every chunk offset on the way
//! past.
//!
//! So every `moov` this crate writes carries slack: a trailing `free` child with
//! [`RESERVED_MOOV_SLACK`] bytes in it. A later edit rebuilds `moov`, re-pads it
//! to exactly the length it had, and writes it back over the old one. The box
//! keeps its size, the media keeps its offsets, and the write is bounded by the
//! header rather than by the file.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::boxutil::{read_box_header, BoxHeader, MOOV};
use crate::error::{Mp4Error, Result};

/// The smallest `free` box: a header and nothing else.
const FREE_BOX_LEN: usize = 8;

/// Slack every `moov` this crate writes carries, as a trailing `free` child.
///
/// A megabyte covers a full tag set, a cover, and a chapter track for a book
/// with several hundred chapters — the things that get added after the media is
/// already on disk. Against the file it protects, which is hundreds of megabytes
/// of audio, it costs about a thousandth; against the alternative, which is
/// rewriting all of that to make room, it costs nothing at all.
pub const RESERVED_MOOV_SLACK: usize = 1 << 20;

/// Where a file's `moov` sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoovLocation {
    /// Byte offset of this box or region within the file.
    pub start: u64,
    /// Total box length, `free` padding included.
    pub len: u64,
    /// Whether `moov` is the last box in the file, in which case it can grow
    /// past its slack for the price of an append.
    pub is_last: bool,
}

/// Read a file's `moov`, and note where it sits.
///
/// Only the box headers and `moov` itself are read, so the cost is the header's
/// and not the file's.
///
/// # Arguments
///
/// * `path` - Filesystem path involved in this operation.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn read_moov(path: &Path) -> Result<(MoovLocation, Vec<u8>)> {
    let mut file = File::open(path)?;
    let boxes = top_level_boxes(&mut file)?;
    let moov = boxes
        .iter()
        .find(|header| header.kind == MOOV)
        .ok_or_else(|| Mp4Error::container("file has no moov"))?;

    let len = usize::try_from(moov.size)
        .map_err(|_| Mp4Error::container("moov too large to rebuild in memory"))?;
    let mut bytes = vec![0u8; len];
    file.seek(SeekFrom::Start(moov.start))?;
    file.read_exact(&mut bytes)?;

    let is_last = boxes.last().is_some_and(|last| last.start == moov.start);
    Ok((
        MoovLocation {
            start: moov.start,
            len: moov.size,
            is_last,
        },
        bytes,
    ))
}

/// Every box at the top level, in file order.
///
/// # Arguments
///
/// * `file` - Open file handle.
///
/// # Returns
///
/// On success, the inner `Vec<BoxHeader>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn top_level_boxes(file: &mut File) -> Result<Vec<BoxHeader>> {
    let file_len = file.seek(SeekFrom::End(0))?;
    let mut out = Vec::new();
    let mut pos = 0u64;
    while pos + 8 <= file_len {
        file.seek(SeekFrom::Start(pos))?;
        let header = read_box_header(file)?;
        if header.end() <= pos {
            return Err(Mp4Error::container(format!(
                "box {} at {pos} does not advance",
                header.kind
            )));
        }
        pos = header.end();
        out.push(header);
    }
    Ok(out)
}

/// Drop a `moov`'s trailing `free` child, if it has one.
///
/// Anything appended to `moov` goes after its last child, so the slack has to
/// come off before an edit and go back on after it. Returns whether one was
/// there to remove.
///
/// # Arguments
///
/// * `moov` - `moov` input for this call.
///
/// # Returns
///
/// On success, the inner `bool` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn strip_trailing_free(moov: &mut Vec<u8>) -> Result<bool> {
    let end = last_child_range(moov)?;
    let Some((start, kind)) = end else {
        return Ok(false);
    };
    if &kind != b"free" && &kind != b"skip" {
        return Ok(false);
    }
    moov.truncate(start);
    set_box_len(moov)?;
    Ok(true)
}

/// Grow `moov` to exactly `total` bytes by appending a `free` child.
///
/// # Errors
///
/// [`Mp4Error::NoRoom`] when `moov` is already longer than `total`, or when the
/// gap is too small to express — a `free` box cannot be shorter than its own
/// header, so one to seven spare bytes are as unusable as none.
///
/// # Panics
///
/// Panics when an internal invariant does not hold.
pub fn pad_moov_to(moov: &mut Vec<u8>, total: usize) -> Result<()> {
    if moov.len() == total {
        return Ok(());
    }
    if total > u32::MAX as usize {
        return Err(Mp4Error::NoRoom {
            needed: moov.len(),
            available: total,
        });
    }
    let gap = total
        .checked_sub(moov.len())
        .filter(|gap| *gap >= FREE_BOX_LEN)
        .ok_or(Mp4Error::NoRoom {
            needed: moov.len(),
            available: total,
        })?;

    moov.reserve(gap);
    moov.extend_from_slice(
        &u32::try_from(gap)
            .expect("gap fits u32 when total does")
            .to_be_bytes(),
    );
    moov.extend_from_slice(b"free");
    moov.resize(total, 0);
    set_box_len(moov)?;
    Ok(())
}

/// Move every chunk offset in `moov` past `above` by `delta` bytes.
///
/// Chunk offsets are absolute file positions, so an edit that shifts the media
/// has to correct them — and an edit that only *appeared* to shift it, because
/// the work happened in a scratch file laid out differently, has to correct them
/// back. Only what sits after the edit moves, which is why `above` is usually
/// where `moov` starts: media in front of the header stays put.
///
/// # Arguments
///
/// * `moov` - `moov` input for this call.
/// * `above` - Numeric `above` value for this call.
/// * `delta` - Numeric `delta` value for this call.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
///
/// # Panics
///
/// Panics when an internal invariant does not hold.
pub fn shift_chunk_offsets(moov: &mut [u8], above: u64, delta: i64) -> Result<()> {
    if delta == 0 {
        return Ok(());
    }
    let mut boxes = Vec::new();
    collect_chunk_offset_boxes(moov, 0, moov.len(), &mut boxes);

    for (start, end, wide) in boxes {
        // FullBox header, then a 4-byte entry count.
        let mut pos = start + 16;
        let stride = if wide { 8 } else { 4 };
        while pos + stride <= end {
            let old = if wide {
                u64::from_be_bytes(moov[pos..pos + 8].try_into().unwrap())
            } else {
                u64::from(u32::from_be_bytes(moov[pos..pos + 4].try_into().unwrap()))
            };
            if old <= above {
                pos += stride;
                continue;
            }
            let new = i64::try_from(old)
                .ok()
                .and_then(|old| old.checked_add(delta))
                .and_then(|new| u64::try_from(new).ok())
                .ok_or_else(|| {
                    Mp4Error::container(format!("chunk offset {old} moved out of range by {delta}"))
                })?;
            if wide {
                moov[pos..pos + 8].copy_from_slice(&new.to_be_bytes());
            } else {
                let narrow = u32::try_from(new).map_err(|_| {
                    Mp4Error::container("chunk offset outgrew stco; the file needs co64")
                })?;
                moov[pos..pos + 4].copy_from_slice(&narrow.to_be_bytes());
            }
            pos += stride;
        }
    }
    Ok(())
}

/// Write `moov` back where the old one was, padding it to the same length.
///
/// The media does not move, so every chunk offset in the file stays true.
///
/// # Errors
///
/// [`Mp4Error::NoRoom`] when the rebuilt `moov` does not fit the space the old
/// one occupied and cannot simply be appended to.
pub fn write_moov_in_place(path: &Path, at: MoovLocation, moov: &[u8]) -> Result<()> {
    let reserved = usize::try_from(at.len)
        .map_err(|_| Mp4Error::container("moov too large to rebuild in memory"))?;

    let mut padded;
    let bytes = if moov.len() == reserved {
        moov
    } else if at.is_last {
        // Nothing follows, so the box is free to end wherever it likes.
        moov
    } else {
        padded = moov.to_vec();
        pad_moov_to(&mut padded, reserved)?;
        &padded
    };

    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    file.seek(SeekFrom::Start(at.start))?;
    file.write_all(bytes)?;
    if at.is_last {
        file.set_len(at.start + bytes.len() as u64)?;
    }
    file.sync_all()?;
    Ok(())
}

/// Boxes that can hold a chunk offset table somewhere beneath them.
const OFFSET_ANCESTORS: &[&[u8; 4]] = &[b"moov", b"trak", b"mdia", b"minf", b"stbl"];

/// Walks `moov` descendants and records `stco`/`co64` byte ranges for offset patching.
fn collect_chunk_offset_boxes(
    buf: &[u8],
    start: usize,
    end: usize,
    out: &mut Vec<(usize, usize, bool)>,
) {
    let mut pos = start;
    while pos + 8 <= end {
        let size = u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
        if size < 8 || pos + size > end {
            return;
        }
        let kind: &[u8; 4] = buf[pos + 4..pos + 8].try_into().expect("four byte slice");
        match kind {
            b"stco" => out.push((pos, pos + size, false)),
            b"co64" => out.push((pos, pos + size, true)),
            _ if OFFSET_ANCESTORS.contains(&kind) => {
                collect_chunk_offset_boxes(buf, pos + 8, pos + size, out);
            }
            _ => {}
        }
        pos += size;
    }
}

/// Where the last immediate child of a box starts, and what it is.
fn last_child_range(buf: &[u8]) -> Result<Option<(usize, [u8; 4])>> {
    if buf.len() < 8 {
        return Err(Mp4Error::container("moov shorter than a box header"));
    }
    let mut pos = 8;
    let mut last = None;
    while pos + 8 <= buf.len() {
        let size = u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
        if size < 8 || pos + size > buf.len() {
            break;
        }
        last = Some((
            pos,
            buf[pos + 4..pos + 8].try_into().expect("four byte slice"),
        ));
        pos += size;
    }
    Ok(last)
}

/// Rewrite a box's own size field from its current length.
fn set_box_len(buf: &mut [u8]) -> Result<()> {
    let len = u32::try_from(buf.len())
        .map_err(|_| Mp4Error::container("moov does not fit a 32-bit box size"))?;
    buf.get_mut(0..4)
        .ok_or_else(|| Mp4Error::container("moov shorter than a box header"))?
        .copy_from_slice(&len.to_be_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + body.len());
        out.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn stco(offsets: &[u32]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        for offset in offsets {
            body.extend_from_slice(&offset.to_be_bytes());
        }
        wrap(b"stco", &body)
    }

    fn moov_with(stco_box: Vec<u8>) -> Vec<u8> {
        let stbl = wrap(b"stbl", &stco_box);
        let minf = wrap(b"minf", &stbl);
        let mdia = wrap(b"mdia", &minf);
        let trak = wrap(b"trak", &mdia);
        wrap(b"moov", &trak)
    }

    fn box_len(buf: &[u8]) -> usize {
        u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize
    }

    #[test]
    fn padding_declares_its_own_length_and_the_boxs() {
        let mut moov = moov_with(stco(&[100]));
        let original = moov.len();
        pad_moov_to(&mut moov, original + 64).unwrap();

        assert_eq!(moov.len(), original + 64);
        assert_eq!(box_len(&moov), original + 64, "moov must cover its padding");
        assert_eq!(&moov[original + 4..original + 8], b"free");
        assert_eq!(box_len(&moov[original..]), 64);
    }

    /// A `free` box cannot be shorter than its header, so a gap of one to seven
    /// bytes is as unusable as none — and has to say so rather than silently
    /// leaving the box the wrong length.
    #[test]
    fn a_gap_too_small_for_a_free_box_is_refused() {
        let mut moov = moov_with(stco(&[100]));
        let original = moov.len();
        for gap in 1..FREE_BOX_LEN {
            let mut candidate = moov.clone();
            assert!(
                matches!(
                    pad_moov_to(&mut candidate, original + gap),
                    Err(Mp4Error::NoRoom { .. })
                ),
                "gap of {gap} should not be expressible"
            );
        }
        assert!(matches!(
            pad_moov_to(&mut moov, original - 1),
            Err(Mp4Error::NoRoom { .. })
        ));
    }

    #[test]
    fn stripping_slack_undoes_padding_exactly() {
        let mut moov = moov_with(stco(&[100]));
        let original = moov.clone();
        let target = moov.len() + 512;
        pad_moov_to(&mut moov, target).unwrap();
        assert!(strip_trailing_free(&mut moov).unwrap());
        assert_eq!(moov, original);
        assert!(
            !strip_trailing_free(&mut moov).unwrap(),
            "there is nothing left to strip"
        );
    }

    #[test]
    fn every_chunk_offset_moves_together() {
        let mut moov = moov_with(stco(&[100, 200, 300]));
        shift_chunk_offsets(&mut moov, 0, -40).unwrap();
        let tail = &moov[moov.len() - 12..];
        assert_eq!(u32::from_be_bytes(tail[0..4].try_into().unwrap()), 60);
        assert_eq!(u32::from_be_bytes(tail[4..8].try_into().unwrap()), 160);
        assert_eq!(u32::from_be_bytes(tail[8..12].try_into().unwrap()), 260);
    }

    /// Slack sits between the offsets and the end of the box, so a shift has to
    /// keep walking past it rather than stopping at the first thing it cannot
    /// descend into.
    #[test]
    fn padding_does_not_hide_the_offsets_behind_it() {
        let mut moov = moov_with(stco(&[100]));
        let target = moov.len() + 32;
        pad_moov_to(&mut moov, target).unwrap();
        shift_chunk_offsets(&mut moov, 0, 8).unwrap();

        let mut stripped = moov.clone();
        strip_trailing_free(&mut stripped).unwrap();
        let tail = &stripped[stripped.len() - 4..];
        assert_eq!(u32::from_be_bytes(tail.try_into().unwrap()), 108);
    }

    /// Media in front of the header does not move when the header changes size,
    /// so its offsets must be left where they are.
    #[test]
    fn offsets_below_the_edit_stay_where_they_are() {
        let mut moov = moov_with(stco(&[40, 900]));
        shift_chunk_offsets(&mut moov, 500, 64).unwrap();
        let tail = &moov[moov.len() - 8..];
        assert_eq!(u32::from_be_bytes(tail[0..4].try_into().unwrap()), 40);
        assert_eq!(u32::from_be_bytes(tail[4..8].try_into().unwrap()), 964);
    }

    #[test]
    fn a_shift_that_would_underflow_an_offset_is_refused() {
        let mut moov = moov_with(stco(&[10]));
        let err = shift_chunk_offsets(&mut moov, 0, -20).unwrap_err();
        assert!(matches!(err, Mp4Error::Container(_)), "{err}");
    }
}
