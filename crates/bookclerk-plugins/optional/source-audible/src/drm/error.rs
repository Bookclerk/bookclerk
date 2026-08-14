use std::path::PathBuf;

use thiserror::Error;

/// Result alias that fails as [`DrmError`] for Adrm/Widevine decrypt.
pub type Result<T> = std::result::Result<T, DrmError>;

#[derive(Debug, Error)]
/// Failures while locating input, credentials, or running native decrypt/remux.
pub enum DrmError {
    #[error("input file missing: {0}")]
    /// Encrypted source file is not on disk at the expected path.
    InputMissing(PathBuf),

    #[error("decrypt output missing: {0}")]
    /// Decrypt finished but the plaintext output path was not created.
    OutputMissing(PathBuf),

    #[error("decrypt requires audible_key + audible_iv (aaxc voucher)")]
    /// AAXC decrypt was requested without both `audible_key` and `audible_iv`.
    MissingCredentials,

    #[error(
        "legacy AAX activation-bytes decrypt is not supported yet; use aaxc key/iv via acquire"
    )]
    /// Legacy AAX activation-bytes decrypt is not implemented; use AAXC key/iv.
    UnsupportedActivationBytes,

    #[error("invalid decrypt key/iv: {0}")]
    /// Key or IV bytes failed validation before native decrypt.
    InvalidKey(String),

    #[error("MP4 parse/remux error: {0}")]
    /// MP4 container parse or remux failed (including exhausted `moov` slack).
    Mp4(String),

    #[error("native decrypt failed: {0}")]
    /// In-process decrypt/transform reported a sample-level failure.
    Native(String),

    #[error("I/O error: {0}")]
    /// Filesystem or read/write error during decrypt.
    Io(#[from] std::io::Error),

    #[error(transparent)]
    /// Unexpected anyhow failure bubbled from a helper.
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
            Mp4Error::NoRoom { needed, available } => Self::Mp4(format!(
                "moov slack exhausted: needed {needed}, available {available}"
            )),
        }
    }
}
