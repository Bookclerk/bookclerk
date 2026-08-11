use std::path::PathBuf;

use thiserror::Error;

/// Result alias for media packaging and worker operations in this crate.
pub type Result<T> = std::result::Result<T, MediaError>;

/// Errors from in-process codecs, MP4 plumbing, and confined media workers.
#[derive(Debug, Error)]
pub enum MediaError {
    /// A declared input path does not exist or is unreachable.
    #[error("input file missing: {0}")]
    InputMissing(PathBuf),

    /// The job reported success but the expected output path was not written.
    #[error("output file missing: {0}")]
    OutputMissing(PathBuf),

    /// ISO-BMFF / MP4 container parse or remux failure.
    #[error("MP4 parse/remux error: {0}")]
    Mp4(String),

    /// Native encode / remux / metadata failure (non-container detail).
    #[error("media processing failed: {0}")]
    Native(String),

    /// A media worker process could not be started, died, or returned a reply
    /// that could not be parsed. Distinct from [`MediaError::Native`] so a
    /// crashed codec is not mistaken for a malformed file.
    #[error("media worker ({job}) failed: {detail}")]
    Worker {
        /// Which operation was running.
        job: &'static str,
        /// What went wrong.
        detail: String,
    },

    /// Isolation is required but unavailable, so the job was refused rather
    /// than run unconfined. Separate from [`MediaError::Worker`] because
    /// nothing went wrong at runtime — the host is misconfigured.
    #[error(
        "refusing to run {job} unconfined: media isolation is required but unavailable ({detail})"
    )]
    NotIsolated {
        /// Which operation was refused.
        job: &'static str,
        /// Why isolation is unavailable.
        detail: String,
    },

    /// Local filesystem I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Catch-all for wrapped media failures.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<bookclerk_mp4::Mp4Error> for MediaError {
    fn from(err: bookclerk_mp4::Mp4Error) -> Self {
        use bookclerk_mp4::Mp4Error;
        match err {
            Mp4Error::Io(io) => Self::Io(io),
            Mp4Error::Container(detail) => Self::Mp4(detail),
            Mp4Error::Transform(detail) => Self::Native(detail),
            Mp4Error::NoRoom { needed, available } => Self::Mp4(format!(
                "moov slack exhausted: needed {needed}, available {available}"
            )),
        }
    }
}
