//! Liberate ingest quality and output formatting knobs.

use serde::{Deserialize, Serialize};

/// Whether / which chapter JSON sidecars to write beside liberated audio.
///
/// Default is [`Self::Off`]. Replaces the old `save_chapter_json` bool +
/// `chapter_layout` pair for *sidecar output* (Audible API fetch layout is
/// chosen separately when chapters are needed for embedding).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ChapterJsonMode {
    /// Do not write chapter JSON sidecars.
    #[default]
    Off,
    /// Write `chapters.flat.json` only.
    Flat,
    /// Write `chapters.tree.json` (nested) only.
    Tree,
    /// Write both flat and tree sidecars.
    Both,
}

impl ChapterJsonMode {
    #[must_use]
    pub fn wants_flat(self) -> bool {
        matches!(self, Self::Flat | Self::Both)
    }

    #[must_use]
    pub fn wants_tree(self) -> bool {
        matches!(self, Self::Tree | Self::Both)
    }

    #[must_use]
    pub fn wants_any(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Parse `off|flat|tree|both`, or legacy bool-ish values (`true`→tree, `false`→off).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "false" | "0" | "no" => Some(Self::Off),
            "flat" => Some(Self::Flat),
            "tree" | "true" | "1" | "yes" => Some(Self::Tree),
            "both" | "all" => Some(Self::Both),
            _ => None,
        }
    }
}

/// Post-download output formatting. Default is enriched M4B (tags + chapters).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    /// Remux/transcode to M4B and embed metadata/chapters (default).
    #[default]
    EnrichedM4b,
    /// Single MP3 file (LAME re-encode).
    SingleMp3,
    /// One MP3 per chapter (after M4B chapter split).
    SplitMp3ByChapter,
    /// Split MP3 by target file size (see `split_mp3_max_mb`).
    SplitMp3BySize,
    /// Opus output (not yet implemented).
    Opus,
    /// Leave store-delivered media as-is (no remux/transcode/re-encode).
    None,
}

impl OutputFormat {
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "enriched_m4b" | "m4b" | "audiobook" => Some(Self::EnrichedM4b),
            "single_mp3" | "mp3" => Some(Self::SingleMp3),
            "split_mp3_by_chapter" | "split_by_chapter" | "chapter_mp3" => {
                Some(Self::SplitMp3ByChapter)
            }
            "split_mp3_by_size" | "split_by_size" => Some(Self::SplitMp3BySize),
            "opus" => Some(Self::Opus),
            "none" | "noop" | "as_is" | "passthrough" => Some(Self::None),
            _ => None,
        }
    }

    #[must_use]
    pub fn wants_mp3(self) -> bool {
        matches!(
            self,
            Self::SingleMp3 | Self::SplitMp3ByChapter | Self::SplitMp3BySize
        )
    }

    #[must_use]
    pub fn wants_split_by_chapter(self) -> bool {
        matches!(self, Self::SplitMp3ByChapter)
    }

    #[must_use]
    pub fn wants_split_by_size(self) -> bool {
        matches!(self, Self::SplitMp3BySize)
    }

    #[must_use]
    pub fn is_noop(self) -> bool {
        matches!(self, Self::None)
    }

    #[must_use]
    pub fn wants_opus(self) -> bool {
        matches!(self, Self::Opus)
    }

    #[must_use]
    pub fn prefers_m4b_container(self) -> bool {
        matches!(self, Self::EnrichedM4b | Self::SplitMp3ByChapter)
    }
}

/// Preferred ingest quality when a store offers multiple encodes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum IngestQuality {
    /// Highest bitrate / quality the store offers (default).
    #[default]
    Highest,
    /// High tier (Audible High, GraphicAudio Hi, …).
    High,
    /// Normal / standard tier (Audible Normal, …).
    Normal,
    /// Lowest / Lo tier when available.
    Low,
}

impl IngestQuality {
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "highest" | "max" | "best" => Some(Self::Highest),
            "high" | "hi" => Some(Self::High),
            "normal" | "standard" | "medium" => Some(Self::Normal),
            "low" | "lo" | "lowest" => Some(Self::Low),
            _ => None,
        }
    }

    /// Map onto Audible's High/Normal license quality.
    #[must_use]
    pub fn as_audible(self) -> crate::settings::AudioQuality {
        match self {
            Self::Highest | Self::High => crate::settings::AudioQuality::High,
            Self::Normal | Self::Low => crate::settings::AudioQuality::Normal,
        }
    }

    /// Prefer GraphicAudio Hi URL unless Low was requested.
    #[must_use]
    pub fn prefers_graphicaudio_hi(self) -> bool {
        !matches!(self, Self::Low)
    }
}

/// Global + optional per-source ingest quality.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct IngestConfig {
    /// Default when a source-specific override is unset.
    pub quality: IngestQuality,
    /// Optional overrides keyed by content source id.
    pub sources: IngestSourceQualities,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            quality: IngestQuality::Highest,
            sources: IngestSourceQualities::default(),
        }
    }
}

impl IngestConfig {
    /// Resolve quality for a content source id (`audible`, `chirp`, …).
    #[must_use]
    pub fn quality_for(&self, source: &str) -> IngestQuality {
        let key = source.trim().to_ascii_lowercase();
        match key.as_str() {
            "audible" => self.sources.audible.unwrap_or(self.quality),
            "libro" | "libro.fm" => self.sources.libro.unwrap_or(self.quality),
            "chirp" => self.sources.chirp.unwrap_or(self.quality),
            "graphicaudio" | "graphic_audio" | "ga" => {
                self.sources.graphicaudio.unwrap_or(self.quality)
            }
            _ => self.quality,
        }
    }
}

/// Per-source ingest quality overrides (`None` → use [`IngestConfig::quality`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct IngestSourceQualities {
    pub audible: Option<IngestQuality>,
    pub libro: Option<IngestQuality>,
    pub chirp: Option<IngestQuality>,
    pub graphicaudio: Option<IngestQuality>,
}

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

    /// Env override `LIBATION_GA_ACCESS` or legacy `LIBATION_GA_FETCH`.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        for key in ["LIBATION_GA_ACCESS", "LIBATION_GA_FETCH"] {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chapter_json_parse() {
        assert_eq!(ChapterJsonMode::parse("off"), Some(ChapterJsonMode::Off));
        assert_eq!(ChapterJsonMode::parse("BOTH"), Some(ChapterJsonMode::Both));
        assert_eq!(ChapterJsonMode::parse("true"), Some(ChapterJsonMode::Tree));
    }

    #[test]
    fn output_parse() {
        assert_eq!(
            OutputFormat::parse("enriched_m4b"),
            Some(OutputFormat::EnrichedM4b)
        );
        assert_eq!(OutputFormat::parse("none"), Some(OutputFormat::None));
        assert_eq!(
            OutputFormat::parse("split_mp3_by_chapter"),
            Some(OutputFormat::SplitMp3ByChapter)
        );
    }

    #[test]
    fn ingest_per_source() {
        let mut cfg = IngestConfig::default();
        assert_eq!(cfg.quality_for("chirp"), IngestQuality::Highest);
        cfg.sources.graphicaudio = Some(IngestQuality::Low);
        assert!(!cfg.quality_for("graphicaudio").prefers_graphicaudio_hi());
        assert_eq!(
            cfg.quality_for("audible").as_audible(),
            crate::settings::AudioQuality::High
        );
    }

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
