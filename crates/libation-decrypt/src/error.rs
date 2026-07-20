use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, DecryptError>;

#[derive(Debug, Error)]
pub enum DecryptError {
    #[error("input file missing: {0}")]
    InputMissing(PathBuf),

    #[error("decrypt output missing: {0}")]
    OutputMissing(PathBuf),

    #[error("decrypt requires audible_key + audible_iv (aaxc voucher)")]
    MissingCredentials,

    #[error(
        "legacy AAX activation-bytes decrypt is not supported yet; use aaxc key/iv via liberate"
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
