use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, DrmError>;

#[derive(Debug, Error)]
pub enum DrmError {
    #[error("input file missing: {0}")]
    InputMissing(PathBuf),

    #[error("decrypt output missing: {0}")]
    OutputMissing(PathBuf),

    #[error("decrypt requires audible_key + audible_iv (aaxc voucher)")]
    MissingCredentials,

    #[error(
        "legacy AAX activation-bytes decrypt is not supported yet; use aaxc key/iv via acquire"
    )]
    UnsupportedActivationBytes,

    #[error("invalid decrypt key/iv: {0}")]
    InvalidKey(String),

    #[error("MP4 parse/remux error: {0}")]
    Mp4(String),

    #[error("native decrypt failed: {0}")]
    Native(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<bookclerk_mp4::Mp4Error> for DrmError {
    fn from(err: bookclerk_mp4::Mp4Error) -> Self {
        use bookclerk_mp4::Mp4Error;
        match err {
            Mp4Error::Io(io) => Self::Io(io),
            Mp4Error::Container(detail) => Self::Mp4(detail),
            // Raised by the shared remuxer or by DASH assembly when a sample
            // cannot be copied/decrypted (not only the decrypt transform).
            Mp4Error::Transform(detail) => Self::Native(detail),
        }
    }
}
