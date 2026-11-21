//! What a stored audio file is: its media type, average bitrate and
//! whether its encoding is lossless.
//!
//! The values come from the embedded-tag reader ([`MediaPool::read_tags`]),
//! so a probe reads only a file's headers, through the confined worker, and
//! works on a local file or a storage plugin's ranged read alike. The
//! library keeps the result on the file's row and compares two copies of
//! one edition with it (library data model ADR, section 3).

use std::io::SeekFrom;
use std::path::{Path, PathBuf};

use bookclerk_tags::{ByteSpan, TagRecord};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

use crate::error::Result;
use crate::pool::MediaPool;
use crate::tag_worker::RangeSource;

/// What a probe found in one audio file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioProbe {
    /// Media type of the container (`audio/mp4`, `audio/mpeg`,
    /// `audio/flac`).
    pub content_type: &'static str,
    /// Average bitrate in kilobits per second: the file's size over its
    /// running time, cover and tags included. `None` when the file states
    /// no running time.
    pub bitrate_kbps: Option<u32>,
    /// True when the audio encoding is lossless (FLAC, ALAC). An MP4 file
    /// whose encoding the reader does not know counts as lossy.
    pub lossless: bool,
    /// The audio encoding, when the tag reader knows it.
    pub codec: Option<bookclerk_tags::AudioCodec>,
    /// Channels, sample rate and AAC profile, as far as the headers state
    /// them.
    pub format: bookclerk_tags::AudioFormat,
    /// The running time a player sees, in milliseconds, as the container
    /// states it (an MP4's movie duration, which edit lists shape; an Opus
    /// stream's samples past its pre-skip). `None` when the file states
    /// none, never zero.
    pub duration_ms: Option<u64>,
}

impl AudioProbe {
    /// The probe of a file of `size` bytes whose tags read as `record`.
    #[must_use]
    pub fn from_record(size: u64, record: &TagRecord) -> Self {
        let bitrate_kbps = record
            .duration
            .filter(|duration| duration.ms > 0)
            .map(|duration| {
                // Bits per millisecond are kilobits per second.
                let kbps = u128::from(size) * 8 / u128::from(duration.ms);
                u32::try_from(kbps).unwrap_or(u32::MAX).max(1)
            });
        Self {
            content_type: record.container.content_type(),
            bitrate_kbps,
            lossless: record
                .codec
                .is_some_and(bookclerk_tags::AudioCodec::is_lossless),
            codec: record.codec,
            format: record.format,
            duration_ms: record
                .duration
                .map(|duration| duration.ms)
                .filter(|&ms| ms > 0),
        }
    }
}

/// A file on this machine, read a range at a time.
#[derive(Debug, Clone)]
pub struct FileRange {
    /// Where the file is.
    path: PathBuf,
}

impl FileRange {
    /// A source over the file at `path`. Nothing is opened until a range is
    /// read.
    #[must_use]
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
        }
    }
}

impl RangeSource for FileRange {
    async fn read_range(&self, span: ByteSpan) -> std::io::Result<Vec<u8>> {
        let len = usize::try_from(span.len).map_err(std::io::Error::other)?;
        let mut file = tokio::fs::File::open(&self.path).await?;
        file.seek(SeekFrom::Start(span.offset)).await?;
        let mut bytes = vec![0; len];
        file.read_exact(&mut bytes).await?;
        Ok(bytes)
    }
}

impl MediaPool {
    /// Probes the audio file at `path` on this machine.
    ///
    /// # Errors
    ///
    /// [`crate::MediaError::Io`] when the file cannot be read, and every
    /// error of [`Self::read_tags`]: the file is not MP4, MP3 or FLAC, or
    /// isolation is required and no worker was found.
    pub async fn probe_file(&self, path: &Path) -> Result<AudioProbe> {
        let size = tokio::fs::metadata(path).await?.len();
        let record = self.read_tags(size, &FileRange::new(path)).await?;
        Ok(AudioProbe::from_record(size, &record))
    }
}

#[cfg(test)]
mod tests {
    use bookclerk_tags::fixtures::{tagged_flac, tagged_m4b, tagged_mp3};
    use bookclerk_tags::read_all;

    use super::*;

    #[test]
    fn a_probe_names_the_type_bitrate_and_losslessness() {
        let m4b = tagged_m4b();
        let record = read_all(&m4b).expect("m4b");
        // 90 s of running time: 8 Mbit over it is 88 kbit/s.
        let probe = AudioProbe::from_record(990_000, &record);
        assert_eq!(probe.content_type, "audio/mp4");
        assert_eq!(probe.bitrate_kbps, Some(88));
        assert!(!probe.lossless);
        assert_eq!(probe.codec, Some(bookclerk_tags::AudioCodec::Aac));
        assert_eq!(
            probe.format.aac_profile,
            Some(bookclerk_tags::AacProfile::Lc)
        );
        assert_eq!(probe.format.channels, Some(2));
        assert_eq!(probe.duration_ms, Some(90_000));
        // A tiny file still reports at least 1 kbit/s.
        assert_eq!(AudioProbe::from_record(1, &record).bitrate_kbps, Some(1));

        let flac = tagged_flac();
        let probe = AudioProbe::from_record(
            u64::try_from(flac.len()).expect("len"),
            &read_all(&flac).expect("flac"),
        );
        assert_eq!(probe.content_type, "audio/flac");
        assert!(probe.lossless);

        let mp3 = tagged_mp3();
        let mut record = read_all(&mp3).expect("mp3");
        assert_eq!(
            AudioProbe::from_record(1000, &record).content_type,
            "audio/mpeg"
        );
        // No running time, no bitrate.
        record.duration = None;
        assert_eq!(AudioProbe::from_record(1000, &record).bitrate_kbps, None);
    }

    #[tokio::test]
    async fn a_local_file_is_probed_through_the_pool() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("book.m4b");
        let file = tagged_m4b();
        tokio::fs::write(&path, &file).await.expect("write");
        let probe = MediaPool::in_process()
            .probe_file(&path)
            .await
            .expect("probe");
        assert_eq!(probe.content_type, "audio/mp4");
        assert!(probe.bitrate_kbps.is_some());
        // A range past the end is an error, not a short read.
        let past = ByteSpan {
            offset: u64::try_from(file.len()).expect("len"),
            len: 8,
        };
        assert!(FileRange::new(&path).read_range(past).await.is_err());
        assert!(MediaPool::in_process()
            .probe_file(&dir.path().join("missing.m4b"))
            .await
            .is_err());
    }
}
