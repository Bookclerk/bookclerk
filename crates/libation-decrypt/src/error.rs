use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, DecryptError>;

#[derive(Debug, Error)]
pub enum DecryptError {
    #[error("input file missing: {0}")]
    InputMissing(PathBuf),

    #[error("decrypt output missing: {0}")]
    OutputMissing(PathBuf),

    #[error("aaxclean-cli not found at {0}; install it or set AUDIBLE_AAXCLEAN_CLI")]
    AaxcleanNotFound(PathBuf),

    #[error("ffmpeg not found at {0}; install it or set LIBATION_FFMPEG")]
    FfmpegNotFound(PathBuf),

    #[error(
        "no CENC decrypt tool available (tried aaxclean-cli at {aaxclean} and ffmpeg at {ffmpeg})"
    )]
    DecryptToolMissing { aaxclean: PathBuf, ffmpeg: PathBuf },

    #[error("aaxclean-cli failed (status={status:?}): {stderr}")]
    AaxcleanFailed { status: Option<i32>, stderr: String },

    #[error("ffmpeg failed (status={status:?}): {stderr}")]
    FfmpegFailed { status: Option<i32>, stderr: String },

    #[error("decrypt requires audible_key + audible_iv (aaxc voucher)")]
    MissingCredentials,

    #[error(
        "legacy AAX activation-bytes decrypt is not supported yet; use aaxc key/iv via liberate"
    )]
    UnsupportedActivationBytes,

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
