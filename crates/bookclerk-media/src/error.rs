use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, MediaError>;

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("input file missing: {0}")]
    InputMissing(PathBuf),

    #[error("output file missing: {0}")]
    OutputMissing(PathBuf),

    #[error("MP4 parse/remux error: {0}")]
    Mp4(String),

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

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

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
