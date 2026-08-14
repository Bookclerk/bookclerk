//! Putting an edited `moov` back into a finished audiobook.
//!
//! Everything added after the media is written — tags, a cover, a chapter track
//! — grows `moov`, and in a faststart file `moov` sits in front of the media.
//! The writer this crate uses leaves slack for exactly that ([`RESERVED_MOOV_SLACK`]),
//! so the usual case is a header-sized write and nothing else moves.
//!
//! When an edit outgrows the slack the file does have to be rebuilt, and this
//! streams it through a scratch file. That matters more than the extra pass: the
//! obvious way to move the media is to read it into a buffer first, and a book
//! is bigger than the memory a small VPS has to spare.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use bookclerk_mp4::{MoovLocation, Mp4Error};

use crate::error::{MediaError, Result};
use crate::mux_aac::IO_BUFFER_BYTES;

/// Replace the `moov` at `at` with `moov`.
///
/// Prefers an in-place swap and falls back to a streaming rebuild, so the caller
/// does not have to know which one it got.
pub(crate) fn write_moov(path: &Path, at: MoovLocation, moov: Vec<u8>) -> Result<()> {
    match bookclerk_mp4::write_moov_in_place(path, at, &moov) {
        Ok(()) => Ok(()),
        Err(Mp4Error::NoRoom { needed, available }) => {
            tracing::info!(
                path = %path.display(),
                needed,
                available,
                "moov outgrew its reserved slack; rebuilding the file"
            );
            rebuild_around_moov(path, at, moov)
        }
        Err(err) => Err(err.into()),
    }
}

/// Copy the file past a `moov` that no longer fits where the old one was.
///
/// Only reachable for a faststart layout: when `moov` is last there is nothing
/// after it to move, so the in-place write always succeeds.
fn rebuild_around_moov(path: &Path, at: MoovLocation, mut moov: Vec<u8>) -> Result<()> {
    let delta = i64::try_from(moov.len())
        .ok()
        .and_then(|new| i64::try_from(at.len).ok().map(|old| new - old))
        .ok_or_else(|| MediaError::Mp4("moov length does not fit an offset delta".into()))?;
    // Every byte after `moov` slides by the same amount, and the chunk offsets
    // that point at it have to follow.
    bookclerk_mp4::shift_chunk_offsets(&mut moov, at.start, delta)?;

    let scratch = crate::package_m4b::scratch_beside(path)?;
    {
        let mut src = BufReader::with_capacity(IO_BUFFER_BYTES, File::open(path)?);
        let mut dst = BufWriter::with_capacity(IO_BUFFER_BYTES, scratch.as_file());
        copy_exact(&mut src, &mut dst, at.start)?;
        dst.write_all(&moov)?;
        src.seek(SeekFrom::Start(at.start + at.len))?;
        std::io::copy(&mut src, &mut dst)?;
        dst.into_inner()
            .map_err(|err| MediaError::Native(format!("flush rebuilt M4B: {err}")))?
            .sync_all()?;
    }
    scratch
        .persist(path)
        .map_err(|err| MediaError::Native(format!("replace {}: {err}", path.display())))?;
    Ok(())
}

/// Copies exactly `len` bytes; errors if the source ends early (truncated M4B header).
fn copy_exact(src: &mut impl Read, dst: &mut impl Write, len: u64) -> Result<()> {
    let copied = std::io::copy(&mut src.take(len), dst)?;
    if copied != len {
        return Err(MediaError::Mp4(format!(
            "file ended {} bytes early while copying its header",
            len - copied
        )));
    }
    Ok(())
}
