//! Preferred Libro.fm download packaging (`[sources.libro] container`).

use serde::{Deserialize, Serialize};

/// Preferred Libro.fm download packaging.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum LibroContainer {
    /// Single M4B when the store offers it (default).
    #[default]
    M4b,
    /// Multi-part ZIP of MP3s.
    Zip,
}

impl LibroContainer {
    /// Parse.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "m4b" | "audiobook" => Some(Self::M4b),
            "zip" | "mp3" | "parts" => Some(Self::Zip),
            _ => None,
        }
    }
}
