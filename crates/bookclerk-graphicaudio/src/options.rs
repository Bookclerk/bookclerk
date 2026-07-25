//! GraphicAudio-native `[sources.graphicaudio]` knobs.

use serde::{Deserialize, Serialize};

/// How GraphicAudio fetches owned audio (`[sources.graphicaudio] access`).
///
/// Default is [`Self::Web`] (Browser Player). ZIP downloads and Access App
/// device registration are opt-in — do not assume the user purchased a ZIP
/// SKU or wants a device slot consumed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum GraphicAudioAccess {
    /// Magento Browser Player (`/library`) — no ZIP attempts, no device slot.
    #[default]
    Web,
    /// Magento ZIP download (`My Downloadable Products`) — ≤3 attempts; opt-in.
    Zip,
    /// Access App API (`/access`) — registers/uses a device activation; opt-in.
    Device,
}

impl GraphicAudioAccess {
    /// Parse a config / env / CLI token (`web`, `zip`, `device`, plus aliases).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "web" | "browser" | "player" | "library" => Some(Self::Web),
            "zip" | "magento" | "m4b" => Some(Self::Zip),
            "device" | "app" | "access" | "api" => Some(Self::Device),
            _ => None,
        }
    }

    /// Env override `BOOKCLERK_GA_ACCESS` or legacy `BOOKCLERK_GA_FETCH`.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        for key in ["BOOKCLERK_GA_ACCESS", "BOOKCLERK_GA_FETCH"] {
            if let Ok(v) = std::env::var(key) {
                if let Some(parsed) = Self::parse(&v) {
                    return Some(parsed);
                }
                if !v.trim().is_empty() && !v.eq_ignore_ascii_case("auto") {
                    tracing::warn!(%key, value = %v, "unknown GraphicAudio access value; ignoring");
                }
            }
        }
        None
    }
}

/// GraphicAudio device encode preference (`[sources.graphicaudio] bitrate`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum GraphicAudioBitrate {
    /// Higher bitrate / Hi URL when available.
    #[default]
    Hi,
    /// Lower bitrate / Lo URL.
    Lo,
}

impl GraphicAudioBitrate {
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "hi" | "high" => Some(Self::Hi),
            "lo" | "low" => Some(Self::Lo),
            _ => None,
        }
    }

    #[must_use]
    pub fn prefers_hi(self) -> bool {
        matches!(self, Self::Hi)
    }
}

/// Preferred GraphicAudio ZIP SKU when [`GraphicAudioAccess::Zip`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum GraphicAudioContainer {
    /// Prefer M4B, then MP3, then FLAC (default).
    #[default]
    Auto,
    M4b,
    Mp3,
    Flac,
}

impl GraphicAudioContainer {
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" | "any" => Some(Self::Auto),
            "m4b" => Some(Self::M4b),
            "mp3" => Some(Self::Mp3),
            "flac" => Some(Self::Flac),
            _ => None,
        }
    }

    /// Rank used when sorting Magento downloadable options (lower = better).
    #[must_use]
    pub fn format_rank(self, option_label: &str) -> u8 {
        let label = option_label.to_ascii_lowercase();
        match self {
            Self::Auto => {
                if label.contains("m4b") {
                    0
                } else if label.contains("mp3") {
                    1
                } else if label.contains("flac") {
                    2
                } else {
                    3
                }
            }
            Self::M4b => {
                if label.contains("m4b") {
                    0
                } else {
                    10
                }
            }
            Self::Mp3 => {
                if label.contains("mp3") {
                    0
                } else {
                    10
                }
            }
            Self::Flac => {
                if label.contains("flac") {
                    0
                } else {
                    10
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphicaudio_access_parse() {
        assert_eq!(
            GraphicAudioAccess::parse("web"),
            Some(GraphicAudioAccess::Web)
        );
        assert_eq!(
            GraphicAudioAccess::parse("browser"),
            Some(GraphicAudioAccess::Web)
        );
        assert_eq!(
            GraphicAudioAccess::parse("zip"),
            Some(GraphicAudioAccess::Zip)
        );
        assert_eq!(
            GraphicAudioAccess::parse("device"),
            Some(GraphicAudioAccess::Device)
        );
        assert_eq!(
            GraphicAudioAccess::parse("app"),
            Some(GraphicAudioAccess::Device)
        );
        assert_eq!(GraphicAudioAccess::parse("auto"), None);
    }
}
