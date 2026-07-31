use thiserror::Error;

pub type Result<T> = std::result::Result<T, Mp4Error>;

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
    NoRoom { needed: usize, available: usize },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl Mp4Error {
    /// Shorthand for [`Mp4Error::Container`].
    pub fn container(detail: impl Into<String>) -> Self {
        Self::Container(detail.into())
    }

    /// Shorthand for [`Mp4Error::Transform`].
    pub fn transform(detail: impl Into<String>) -> Self {
        Self::Transform(detail.into())
    }
}
