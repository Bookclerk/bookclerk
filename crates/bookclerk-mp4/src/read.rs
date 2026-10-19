//! Buffered reads of sample payloads out of a file.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::{Mp4Error, Result};

/// Read buffer for the sample copy.
///
/// A sample is a few hundred bytes, so reading one at a time straight from the
/// file would cost a syscall each — a couple of million for an audiobook — where
/// a buffer this size makes it a couple of thousand.
pub(crate) const IO_BUFFER_BYTES: usize = 1 << 20;

/// Reads sample payloads by absolute file offset, buffered.
///
/// Samples sit in track order in a progressive `mdat`, so consecutive reads are
/// usually already in place, and otherwise a short hop ahead. Both cases are
/// served out of the buffer; only a jump past it costs a real seek. Reading in
/// some other order still works, just without the benefit.
#[derive(Debug)]
pub struct SampleReader {
    /// 1 MiB buffered file handle; consecutive `mdat` samples usually stay in-buffer.
    src: BufReader<File>,
    /// Absolute file offset of the next unread byte (kept in sync after each read/seek).
    pos: u64,
    /// File length when the reader was created, or `None` when it could not be
    /// read. Sample sizes come from the file's own tables, so a read is checked
    /// against this before its buffer is allocated.
    len: Option<u64>,
}

impl SampleReader {
    /// Opens `path` for buffered sample reads.
    ///
    /// # Arguments
    ///
    /// * `path` - Filesystem path to a progressive MP4/M4A/M4B.
    ///
    /// # Returns
    ///
    /// Reader positioned at byte 0.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Mp4Error::Io`] when the file cannot be opened.
    pub fn open(path: &Path) -> Result<Self> {
        let path = crate::fs_path::validated(path)?;
        Ok(Self::new(File::open(path)?))
    }

    /// Wraps an already-opened file with a buffered sample reader.
    ///
    /// # Arguments
    ///
    /// * `file` - Seekable file handle positioned anywhere (reader seeks per sample).
    ///
    /// # Returns
    ///
    /// Buffered reader ready for [`Self::read_sample`].
    #[must_use]
    pub fn new(file: File) -> Self {
        let len = file.metadata().ok().map(|meta| meta.len());
        Self {
            src: BufReader::with_capacity(IO_BUFFER_BYTES, file),
            pos: 0,
            len,
        }
    }

    /// Fill `buf` with exactly `size` bytes from `offset`, resizing it to match.
    ///
    /// # Arguments
    ///
    /// * `offset` - Absolute byte offset in the file.
    /// * `size` - Number of bytes to read.
    /// * `buf` - Destination buffer resized to `size`.
    ///
    /// # Returns
    ///
    /// The successful result value for this operation.
    ///
    /// # Errors
    ///
    /// Returns [`Mp4Error::Container`] when the sample would end past the end of
    /// the file (a forged or truncated sample table), and an I/O error when the
    /// read itself fails.
    pub fn read_sample(&mut self, offset: u64, size: usize, buf: &mut Vec<u8>) -> Result<()> {
        if let Some(len) = self.len {
            let end = u64::try_from(size)
                .ok()
                .and_then(|size| offset.checked_add(size));
            match end {
                Some(end) if end <= len => {}
                _ => {
                    return Err(Mp4Error::container(format!(
                        "sample of {size} bytes at offset {offset} runs past the end of the \
                         {len}-byte file"
                    )));
                }
            }
        }
        buf.resize(size, 0);
        self.seek_to(offset)?;
        self.src.read_exact(buf)?;
        self.pos += size as u64;
        Ok(())
    }

    /// Seeks only when `target` differs from `pos`, preferring a relative hop to keep the buffer.
    fn seek_to(&mut self, target: u64) -> Result<()> {
        if self.pos == target {
            return Ok(());
        }
        match (i64::try_from(self.pos), i64::try_from(target)) {
            // Relative, so a gap already inside the buffer keeps the read-ahead.
            (Ok(from), Ok(to)) => self.src.seek_relative(to - from)?,
            // Unreachable short of an 8 EiB file, but an offset that cannot be
            // expressed as a delta is worth an absolute seek, not a wrong one.
            _ => {
                self.src.seek(SeekFrom::Start(target))?;
            }
        }
        self.pos = target;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn file_of(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        File::create(&path).unwrap().write_all(bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn reads_follow_offsets_in_any_order() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        let (_dir, path) = file_of(&bytes);
        let mut reader = SampleReader::open(&path).unwrap();
        let mut buf = Vec::new();

        // Sequential, then a jump back, then forward again: each read must land
        // on its own offset regardless of what came before.
        for (offset, size) in [(0, 4), (4, 4), (200, 8), (8, 2), (250, 6)] {
            reader.read_sample(offset, size, &mut buf).unwrap();
            let start = offset as usize;
            assert_eq!(buf, &bytes[start..start + size], "at offset {offset}");
        }
    }

    #[test]
    fn a_read_past_the_end_is_an_error() {
        let (_dir, path) = file_of(&[1, 2, 3, 4]);
        let mut reader = SampleReader::open(&path).unwrap();
        let mut buf = Vec::new();
        assert!(reader.read_sample(2, 8, &mut buf).is_err());
    }

    #[test]
    fn a_forged_size_is_refused_before_allocating() {
        // A sample table can claim a 4 GiB sample in a tiny file; that used to
        // allocate the whole buffer before the read failed (found by the
        // `mp4_parse` fuzz target).
        let (_dir, path) = file_of(&[1, 2, 3, 4]);
        let mut reader = SampleReader::open(&path).unwrap();
        let mut buf = Vec::new();
        let err = reader
            .read_sample(0, u32::MAX as usize, &mut buf)
            .unwrap_err();
        assert!(matches!(err, Mp4Error::Container(_)), "{err}");
        assert!(buf.capacity() < 1024);
        assert!(reader.read_sample(u64::MAX, 1, &mut buf).is_err());
    }

    #[test]
    fn reads_span_more_than_one_buffer_refill() {
        let bytes: Vec<u8> = (0..(IO_BUFFER_BYTES * 2 + 512))
            .map(|i| (i % 251) as u8)
            .collect();
        let (_dir, path) = file_of(&bytes);
        let mut reader = SampleReader::open(&path).unwrap();
        let mut buf = Vec::new();

        let size = 400;
        let mut offset = 0u64;
        while offset as usize + size <= bytes.len() {
            reader.read_sample(offset, size, &mut buf).unwrap();
            let start = offset as usize;
            assert_eq!(buf, &bytes[start..start + size], "at offset {offset}");
            offset += size as u64;
        }
    }
}
