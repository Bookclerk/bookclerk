use thiserror::Error;

/// Result alias for this crate's error type.
pub type Result<T> = std::result::Result<T, Mp4Error>;

/// Errors from MP4 parse, remux, and in-place patch operations.
#[derive(Debug, Error)]
pub enum Mp4Error {
    /// The file does not match the shape this crate can read or write.
    #[error("MP4 parse/remux error: {0}")]
    Container(String),

    /// A [`crate::SampleTransform`] refused a payload. The crate never produces
    /// this itself; it carries the caller's reason out through the remuxer.
    #[error("sample transform failed: {0}")]
    Transform(String),

    /// A rebuilt `moov` outgrew the space reserved for it, so it cannot be
    /// swapped in without moving the media. Callers fall back to a rewrite.
    #[error("moov needs {needed} bytes but only {available} are reserved")]
    NoRoom {
        /// Bytes required for the rebuilt `moov`.
        needed: usize,
        /// Bytes currently reserved for in-place swap.
        available: usize,
    },

    /// Underlying filesystem or stream I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl Mp4Error {
    /// Shorthand for [`Mp4Error::Container`].
    ///
    /// # Arguments
    ///
    /// * `detail` - Human-readable error detail.
    ///
    /// # Returns
    ///
    /// Updated `Self` for chaining.
    pub fn container(detail: impl Into<String>) -> Self {
        Self::Container(detail.into())
    }

    /// Shorthand for [`Mp4Error::Transform`].
    ///
    /// # Arguments
    ///
    /// * `detail` - Human-readable error detail.
    ///
    /// # Returns
    ///
    /// Updated `Self` for chaining.
    pub fn transform(detail: impl Into<String>) -> Self {
        Self::Transform(detail.into())
    }
}
