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

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
